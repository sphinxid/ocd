# 01 — CLI, Global Paths, Configuration, Background Service

Behavioral specification of the opencode v2 command line (`packages/cli`), the shared
utility package (`packages/util`), and configuration loading (`packages/core/src/config*`,
`packages/schema`). Source revision: `@opencode/cli` `2.0.26` (`packages/cli/package.json:4`).

All source citations are relative to `packages/` of the upstream checkout unless stated
otherwise. Quoted strings are verbatim; `⏎` / `EOL` means `os.EOL` (`\n` on POSIX, `\r\n` on
Windows). Every handler that writes "`... + EOL`" uses `os.EOL`, while hard-coded `"\n"` is
always LF; this distinction is preserved below.

---

## 0. Conventions and build-time constants

### 0.1 Build constants (`cli/src/version.ts:1-10`, `cli/script/build.ts:187-194`)

| Constant | Release build value | Dev (unbundled) fallback | Meaning |
|---|---|---|---|
| `OPENCODE_VERSION` | e.g. `2.0.26`; preview builds `0.0.0-<channel>-<buildNumber>` (`script/src/index.ts:40`) | `"local"` | Reported version, used for service version gating |
| `OPENCODE_CHANNEL` | `latest` for releases; otherwise the sanitized git branch / `$OPENCODE_CHANNEL` at build (`script/src/index.ts:26-36`) | `"local"` | Selects service file names, port, DB name, log name |
| `OPENCODE_ARTIFACT` | `cli` | `cli` | Client name sent to server/telemetry when `OPENCODE_CLIENT` unset |
| `OPENCODE_CLI_NAME` | `opencode` | `opencode` | Root command name used in help/usage (`cli/src/commands/commands.ts:38`) |
| `OPENCODE_LOCAL` | `channel === "local"` | `true` | Disables auto-update; selects `-local` file names |
| `OPENCODE_LIBC` | `glibc`/`musl` on linux | undefined | Parcel watcher native package selection (`core/src/filesystem/watcher-binding.ts:3-12`) |

"Release channels" = `latest`, `dev`, `beta`, `next` throughout (they share files/ports). The DB
name additionally treats `prod` as a release channel (§5).

`sanitize(channel)` everywhere means `channel.replace(/[^a-zA-Z0-9._-]/g, "-")`.

### 0.2 Launcher shim (`cli/bin/opencode.cjs`)

The npm package ships a Node shim (`bin/opencode.cjs`, `bin/opencode2.cjs` just requires it).
A Rust port that ships a native binary does not need it, but for parity:

1. If `$OPENCODE_BIN_PATH` is set, exec that path (`opencode.cjs:33`).
2. Else if a cached sibling file `.<command>` exists next to the real script path, exec it.
3. Else walk up from the script dir looking in each `node_modules/` for packages named
   `@opencode/cli-<platform>-<arch>[-baseline][-musl]` (order depends on AVX2 support and musl
   detection, `opencode.cjs:83-109`) containing `bin/opencode[.exe]`.
4. If not found: stderr `It seems that your package manager failed to install the right opencode CLI package. Try manually installing "<name1>" or "<name2>" ... package`, exit 1.
5. Child is spawned with inherited stdio; SIGINT/SIGTERM/SIGHUP (+SIGUSR1 on POSIX) are
   forwarded; shim exits with child's code, or re-raises the child's terminating signal.

### 0.3 Process startup sequence (`cli/src/index.ts`)

1. `ensurePluginRuntime()` (plugin runtime bootstrap).
2. **SSH askpass mode**: if `OPENCODE_SSH_ASKPASS_PORT` is set, the binary acts as an
   `SSH_ASKPASS` helper and never parses CLI args (`index.ts:23-26`, `ssh-askpass.ts`):
   - Port must parse as int in `1..65535`.
   - Connect TCP `127.0.0.1:<port>`, write one line:
     `{"token":$OPENCODE_SSH_ASKPASS_TOKEN,"text":<argv[2..] joined by " ">,"confirm":<SSH_ASKPASS_PROMPT==="confirm">}` + `\n`.
   - Read until socket close; decode as JSON `{ "value": string | null }`.
   - `value === null` → exit 1. Otherwise write `value + "\n"` to stdout, exit 0.
   - Any error or 5-minute timeout → exit 1. No logging in this mode.
3. Install SIGUSR1 heap-snapshot and SIGPROF CPU-profile listeners (non-Windows, §16.4).
4. Register `uncaughtException` / `unhandledRejection` handlers that only log
   (`"uncaught exception"`, `"unhandled rejection"`).
5. Log `cli starting` with `{version, channel, local, args}` (args = argv after the binary).
6. Parse and dispatch the command (§1). Layers provided: CLI config service (`cli.json`, see
   §6), updater, Global paths (with `config` overridden by
   `$OPENCODE_CONFIG_DIR` when set), process/npm/flock services, Observability (§16).
7. On success: `process.exit(process.exitCode ?? 0)` (`index.ts:139`).
8. On failure: log `cli process failed` `{cause, args}`; if the failure is not
   interrupt-only and the error is "reportable" (all errors except CLI `ShowHelp`), write
   `Cause.pretty(cause) + "\n"` to **stderr** (`index.ts:143-149`). Rationale in source: the
   desktop app and `Service.ensure` only capture stderr of `serve --service`.

### 0.4 Exit codes

| Code | When |
|---|---|
| 0 | Success (or a handler that printed its own message and did not set `exitCode`) |
| 1 | Unhandled handler failure; CLI parse error / missing subcommand (help printed); handlers that catch errors and set `process.exitCode = 1` |
| 130 | Interrupt-only failure (Ctrl-C) per Effect default teardown; interactive prompt cancelled (`handlePromptErrors`, `session export` picker) |
| child code | `acp`: 1 if failure reason, else 0 (explicit `process.exit`) |

### 0.5 CLI framework behavior (Effect `effect/cli` 4.0.x)

The CLI is declared with Effect's CLI module (`cli/src/commands/commands.ts`). Observable
framework behavior the port must replicate:

- **Built-in global flags** on every command: `--help, -h` ("Show help information"),
  `--version, -v` ("Show version information"), `--completions <bash|zsh|fish|sh>`
  ("Print shell completion script"; `sh` = bash), `--log-level <all|trace|debug|info|warn|warning|error|fatal|none>`
  ("Sets the minimum log level"). Plus the app global flag `--print-logs` (§2).
  The first present action flag (help, version, completions) runs and the command exits 0.
- `--version` prints `<name> v<version>` (with ANSI bold/dim when stdout is a TTY and
  `NO_COLOR !== "1"`): e.g. `opencode v2.0.26`.
- Help layout (stdout), sections separated by blank lines, each indented two spaces:
  `DESCRIPTION`, `USAGE` (e.g. `opencode debug paths [flags]`), `ARGUMENTS` (name, type, description,
  `" (optional)"` suffix; variadic names suffixed `...`), `FLAGS` (`--name, -a <type>` then
  description; boolean flags show no type), `GLOBAL FLAGS`, `SUBCOMMANDS` (`name, alias` and description).
  Table column = longest left cell + 4 (arguments capped at 25, subcommands at 20).
- A command group invoked without a subcommand (e.g. `opencode auth`) prints its help to
  stdout and exits 1. Parse/validation errors print help to stdout and then to stderr:
  `\nERROR\n  <message>` (or `\nERRORS\n  <msg>\n  <msg>` for several), exit 1.
- Boolean flags accept `--flag`, `--flag=true|false`, and negation `--no-flag`.
  `Flag.optional` booleans (e.g. `mini --replay`) are tri-state (absent / true / false).
- Repeatable flags: `Flag.atMost(100)` / `Flag.atLeast(0)` collect repeats into an array.
  `Flag.KeyValuePair` collects `--param k=v` occurrences into a record.
- `--` operands: the framework "strands" operands after `--` on the root; `run` and `mcp add`
  therefore re-read `process.argv` after the first `--` (§3).
- Aliases: `upgrade` has alias `update` (shown as `upgrade, update` in help).

---

## 1. Command tree (exact declarations)

Source: `cli/src/commands/commands.ts`. Handlers: `cli/src/index.ts:28-82` (lazy imports
from `cli/src/commands/handlers/**`).

Shared parameter groups:

- **ServerParams** (`commands.ts:15-24`):
  - `--standalone` boolean, default false — "Run with a private server instead of the background service"
  - `--server <string>` optional — "Connect to a server URL instead of the background service"
- **PermissionParams** (`commands.ts:26-36`):
  - `--auto` boolean, default false — "Auto-approve permissions that are not explicitly denied"
  - `--yolo` boolean, default false, **hidden**
  - `--dangerously-skip-permissions` boolean, default false, **hidden**
  - Effective auto = `auto || yolo || dangerouslySkipPermissions`.

| Command | Description (verbatim) | Positional args | Flags |
|---|---|---|---|
| `opencode` (root, `$`) | `OpenCode command line interface` | `directory` optional — "Directory to start OpenCode in" | ServerParams, PermissionParams, `--continue, -c` bool "Continue the last session"; `--session, -s <string>` "Session ID to continue, or to create if it does not exist"; `--prompt <string>` "Prompt to use" |
| `upgrade` (alias `update`) | `Upgrade OpenCode to the latest or a specific version` | `target` optional — "Version to upgrade to (with or without a leading v)" | `--method, -m <choice of Updater.methods>` optional "Installation method to use" |
| `uninstall` | `Uninstall OpenCode, keeping session data, configuration, and state` | – | `--dry-run` bool "Show what would be removed without removing"; `--force, -f` bool "Skip confirmation prompts" |
| `acp` | `Start an Agent Client Protocol server` | – | `--login` bool "Run auth login instead of starting the server" |
| `api` | `Make a request to the running server` | `operation \| method path` variadic min 1 max 2 — "OpenAPI operation ID, or an HTTP method followed by a path" | ServerParams; `--data, -d <string>` "Request body"; `--header, -H <string>` repeat ≤100 "Request header in name:value form"; `--param <key=value>` key-value "OpenAPI path or query parameter" |
| `debug` | `Debugging and troubleshooting tools` | – | – |
| `debug agents` | `List all agents` | – | – |
| `debug config` | `List configuration sources` | – | – |
| `debug paths` | `Show global paths (data, config, cache, state)` | `name` optional, one of `db home data config cache state tmp bin log repos` — "Print only one path: db, home, data, config, cache, state, tmp, bin, log, repos" | – |
| `auth` | `manage integrations and credentials` | – | – |
| `auth list` | `list integrations and credentials` | – | ServerParams; `--format <default\|json>` default `default` "Output format" |
| `auth login` | `connect an integration` | `target` optional — "Integration ID, name, or well-known provider URL" | ServerParams; `--method <string>` "Authentication method ID"; `--answer <string>` repeat ≤100 "Provider form answer (key=value; repeat for multiple fields)" |
| `auth logout` | `log out of a saved account` | `target` optional "Integration ID or name"; `credential` optional "Credential ID or label (opens an account picker when omitted)" | ServerParams |
| `auth export` | `print stored credentials, including secrets, as JSON` | `target` optional "Integration ID or name (exports every integration when omitted)" | ServerParams |
| `auth import` | `import credentials exported by auth export` | `file` optional "JSON file to import (reads stdin when omitted)" | ServerParams |
| `auth switch` | `switch the active account for an integration` | `target` optional "Integration ID or name"; `credential` optional "Credential ID or label (opens an account picker when omitted)" | ServerParams |
| `mcp` | `Manage MCP (Model Context Protocol) servers` | – | – |
| `mcp list` | `List configured MCP servers and their status` | – | – |
| `mcp add` | `Add an MCP server to your configuration` | `name` required "Name of the MCP server"; `command` variadic min 0 "Command and arguments for a local server, passed after --" | `--url <string>` "URL for a remote MCP server"; `--header <k=v>` "HTTP header for a remote server, as name=value"; `--env <k=v>` "Environment variable for a local server, as name=value"; `--global` bool "Write to the global config instead of the project config" |
| `mcp auth` | `Authenticate with an OAuth-capable remote MCP server` | `name` optional "Name of the MCP server" | – |
| `mcp logout` | `Remove stored OAuth credentials for an MCP server` | `name` required "Name of the MCP server" | – |
| `plugin` | `Manage plugins` | – | – |
| `plugin list` | `List plugins` | – | `--builtin` bool "Include built-in server plugins" |
| `plugin add` | `Install a plugin and add it to the global configuration` | `package` required "npm registry or Git package specifier" | – |
| `plugin check` | `Check package plugins for updates` | `target` optional "Configured package target" | – |
| `plugin update` | `Update package plugins` | `target` optional "Configured package target; omit to update all outdated plugins" | – |
| `plugin remove` | `Remove a plugin from global configuration` | `package` required "configured package specifier" | – |
| `models` | `List all available models` | – | ServerParams |
| `stats` | `Show shareable usage statistics` | – | ServerParams; `--days <int ≥0>` "Show the last N days; 0 means today"; `--year <int 1970..9999>` "Show a calendar year"; `--all` "Show lifetime statistics"; `--project <string>` 'Filter by project ID, or use "." for the current project'; `--models` "Show model usage"; `--tools` "Show tool reliability"; `--cost` "Show cost and token details"; `--full` "Show every detailed section"; `--limit <int ≥1>` default 5 "Number of rows in detailed sections"; `--json` "Output statistics as JSON" |
| `mini` | `Start the minimal interactive interface` | – | ServerParams; `--continue, -c`; `--session, -s`; `--fork` "Fork the session when continuing"; `--replay` optional bool "Restore session history on resume and resize (disable with --no-replay)"; `--replay-limit <int>` "Limit replay to the newest N messages (default: 200)"; `--model, -m` "Model to use in the format provider/model"; `--agent` "Agent to use"; `--prompt` "Prompt to use"; `--demo` hidden bool |
| `run` | `Run OpenCode with a message` | `message` variadic min 0 "Message to send" | ServerParams; `--continue, -c`; `--session, -s`; `--fork` "Fork the session before continuing"; `--model, -m` "Model to use in the format provider/model#variant"; `--agent` "Agent to use"; `--format <default\|json>` default `default` "Output format"; `--file, -f <string>` repeat ≤100 "File to attach to the message"; `--title <string>` "Session title"; `--thinking` "Show thinking blocks"; PermissionParams |
| `session` | `Manage sessions` | – | – |
| `session list` | `List top-level sessions in the current project, newest first` | – | ServerParams; `--max-count, -n <int ≥1>` "Limit to N most recent sessions (default: 100)"; `--format <table\|json>` default `table` "Output format" |
| `session delete` | `Delete a session and its child sessions` | `sessionID` required "Session ID to delete" | ServerParams |
| `session export` | `Export session data as JSON` | `session` optional "Session ID to export" | ServerParams; `--sanitize` "Redact sensitive transcript and file data" |
| `session import` | `Import session data from a JSON file or URL` | `file` required "JSON file or URL to import" | ServerParams; `--directory <string>` "Directory in which to import the session" |
| `service` | `Manage the background server` | – | – |
| `service start` | `Start the background server` | – | – |
| `service restart` | `Restart the background server` | – | – |
| `service status` | `Show background server status` | – | – |
| `service stop` | `Stop the background server` | – | – |
| `service get` | `Get service configuration` | `key` optional "Service setting or env"; `name` optional "Environment variable name" | – |
| `service set` | `Set service configuration` | `key` "Service setting or env"; `value` "Setting value or environment variable name"; `env-value` optional "Environment variable value" | – |
| `service unset` | `Unset service configuration` | `key` "Service setting or env"; `name` optional "Environment variable name" | – |
| `reload` | `Reload configuration` | – | ServerParams |
| `pair` | `Print one-time links to connect a browser or app` | – | `--url <string>` "Use an external HTTP(S) server URL in pairing links" (validated, see below); `--remote` bool "Pair through the OpenTunnel remote address, enabling remote access if needed" |
| `serve` | `Start the v2 API and web server` | – | `--hostname <string>`; `--port <int>`; `--cors <non-empty string>` repeatable "Additional allowed CORS origin (repeat for multiple origins)"; `--service` bool (no description); `--stdio` bool (no description) |

`pair --url` validation (`commands.ts:522-533`): parsed with WHATWG `URL`; rejected unless
protocol is `http:`/`https:` and there is no username, password, query (`search`) or fragment.
On rejection the parse error is
`Expected an HTTP(S) server URL without credentials, query parameters, or a fragment`. The
accepted value is `url.href` with trailing slashes stripped (`/\/+$/`).

`Updater.methods` (the `--method` choices) are defined in `cli/src/services/updater.ts`; see the
upgrade section (§11).

---

## 2. Global flag `--print-logs`

`cli/src/commands/commands.ts:6-11`, `cli/src/framework/runtime.ts:304-307`.

- Boolean, default false, description `Print logs to stderr (server logs require --standalone)`.
- Applied to every command. Before a handler module is loaded, if set, the process sets
  `OPENCODE_PRINT_LOGS=1` in its own environment.
- Effect: the stderr logger (§16.1) writes every subsequent log record (same logfmt line
  format as the file) to stderr. Records logged before argument parsing (e.g. `cli starting`)
  are not printed. Child processes inherit the env var: a `--standalone` child server
  inherits stderr when `OPENCODE_PRINT_LOGS=1` (otherwise its stderr is discarded), which is
  why "server logs require --standalone" (the background service's stderr is not attached
  to the terminal).
- Setting `OPENCODE_PRINT_LOGS=1` in the environment has the same effect without the flag.

---

## 3. Handler behavior for core commands

Commands covered elsewhere in this document: `$` (§8), `serve` and `service *` (§7),
`run` / `stats` / `upgrade` / `uninstall` / `mini` / `pair` / `reload` / `models` / `api`
(§9–§12), `acp` (§14).

### 3.1 Server resolution policy per command

All client commands obtain a server via `ServerConnection.resolve(args)` (§7.6). The
version-mismatch policy differs:

| Command(s) | `--server`/`--standalone` honored | Mismatch policy |
|---|---|---|
| `$` (TUI), `mini` | yes | `replace` (restart service if its version ≠ this binary) |
| `run`, `models`, `reload`, `stats`, `session *`, `auth *` | yes | `ignore` (reuse any running compatible service) |
| `api` | yes | `ignore` (explicit) |
| `debug agents`, `debug config`, `mcp list/auth/logout`, `plugin list/check/update` | no (always background service) | `ignore` |
| `pair` | no | ensures service with no version requirement |
| `acp`, `run/v1` | always a private standalone server | n/a |
| `debug paths`, `mcp add`, `plugin add/remove`, `service get/set/unset` | no server needed (plugin add/remove still use npm and the CLI config) | n/a |

Requests use HTTP Basic auth `opencode:<password>` (`Authorization: Basic base64("opencode:"+pw)`,
`client/src/service-probe.ts:93-98`). Most endpoints take the working directory as
query parameter `location[directory]=<cwd>` (nested objects are encoded as
`key[child]=value`, arrays as repeated keys, `null` as the string `null`;
`client/src/promise/generated/client.ts:2248-2263`).

Client method → HTTP endpoint (subset used by the CLI; from the generated client):

| Client call | HTTP |
|---|---|
| `server.info` | `GET /api/info` |
| `server.pair` | `POST /api/pair` |
| `location.get` | `GET /api/location` |
| `location.reload` | `POST /api/location/reload` |
| `agent.list` | `GET /api/agent` |
| `config.get` | `GET /api/config` |
| `session.list` | `GET /api/session` |
| `session.remove` | `DELETE /api/session/{id}` |
| `session.export` | `GET /api/experimental/session/{id}/export` |
| `session.import` (raw fetch) | `POST /api/experimental/session/import` |
| `model.list` / `model.default` | `GET /api/model` / `GET /api/model/default` |
| `integration.list` / `.get` | `GET /api/integration` / `GET /api/integration/{id}` |
| `integration.wellknown.add` | `POST /api/experimental/integration/wellknown` |
| `integration.connect.key` / `.external` | `POST /api/integration/{id}/connect/key` / `/connect/external` |
| `integration.oauth.connect` / `.status` / `.complete` / `.cancel` | `POST /api/integration/{id}/connect/oauth`, `GET …/connect/oauth/{attempt}`, `POST …/{attempt}/complete`, `DELETE …/{attempt}` |
| `integration.command.connect` / `.status` / `.cancel` | `POST /api/integration/{id}/connect/command`, `GET …/{attempt}`, `DELETE …/{attempt}` |
| `credential.list` / `.create` / `.activate` / `.remove` | `GET /api/credential`, `POST /api/credential`, `POST /api/credential/{id}/activate`, `DELETE /api/credential/{id}` |
| `mcp.list` | `GET /api/mcp` |
| `plugin.list` / `.check` / `.update` | `GET /api/plugin`, `POST /api/plugin/check`, `POST /api/plugin/update` |
| `experimental.persistentPty.shutdown` / `.handoff` | `POST /api/experimental/persistent-pty/shutdown` / `…/handoff` |

### 3.2 `debug paths` (`handlers/debug/paths.ts`)

Does not contact or start any server (test: `cli/test/debug-paths.test.ts:17-49`).

- `paths = { ...Global, db: databasePath(global.data) }`; key order:
  `home, data, cache, config, state, tmp, bin, log, repos, db`.
- With `name`: print `paths[name] + EOL`.
- Without: for each entry print `key.padEnd(10) + " " + value + EOL`. Example (Linux):

```
home       /home/u
data       /home/u/.local/share/opencode
cache      /home/u/.cache/opencode
config     /home/u/.config/opencode
state      /home/u/.local/state/opencode
tmp        /tmp/opencode
bin        /home/u/.cache/opencode/bin
log        /home/u/.local/share/opencode/log
repos      /home/u/.local/share/opencode/repos
db         /home/u/.local/share/opencode/opencode.db
```

Note: global directories are created (mkdir -p) as a side effect of acquiring Global (§4).

### 3.3 `debug agents` (`handlers/debug/agents.ts`)

`GET /api/agent?location[directory]=<cwd>`; print `JSON.stringify(data sorted by id via localeCompare, null, 2) + EOL`.

### 3.4 `debug config` (`handlers/debug/config.ts`, `redact.ts`)

`GET /api/config?location[directory]=<cwd>`; print `JSON.stringify(redactConfig(entries), null, 2) + EOL`.

`redactConfig(value, headers=false)` (recursive):
- arrays: map elements.
- non-objects: unchanged.
- object entry `(key, item)`:
  - if `item` is a string and (`headers` is true or key matches
    `/(?:api.?key|secret|password|token$|authorization$|cookie$|credential|private.?key)/i`) → `"***"`.
  - else if `item` is a string matching `/^https?:\/\//i`: if it does not parse as URL → `"***"`;
    if URL has username, password, or any query parameter name matching the secret regex → `"***"`.
  - else recurse with `headers || key.toLowerCase() === "headers"` (every string value under a
    `headers` object is redacted).

### 3.5 `auth list` (`handlers/auth/list.ts`)

1. Resolve server; `loadIntegrations` = `GET /api/model/default` (readiness barrier, result
   ignored) then `GET /api/integration`, sorted so `opencode-go` comes first (stable otherwise)
   (`auth/shared.ts:17-25`). Keep integrations with ≥1 connection.
2. `--format json`: `JSON.stringify(integrations.map(i => ({id, name, connections})), null, 2) + EOL`.
3. Default: rows per connection: `integration=name`, `source = connection.label` (type
   `credential`) or `connection.name` (type `env`), `type = "stored"` or `"environment"`.
   Empty → `No authenticated integrations` + EOL. Else
   `width = max(name length) + 2`; each line `name.padEnd(width) + source.padEnd(28) + type`, joined by EOL, trailing EOL.
4. Any error → stderr `errorMessage(error) + EOL`, exit code 1.

`errorMessage(e)`: `e.message` for Error or objects with string `message`, else `String(e)` (`cli/src/util/error.ts`).

### 3.6 `auth login` (`handlers/auth/login.ts`, `form.ts`, `shared.ts`)

Interactive (clack prompts on stdout). Flow:
1. If no `target` and stdin/stdout are not both TTYs → fail
   `Pass an integration ID or name when running without an interactive terminal`.
2. `intro("Connect an integration")`.
3. If `target` parses as an `http:`/`https:` URL: spinner `Discovering authentication provider...`,
   `POST /api/experimental/integration/wellknown {url, location}`; success
   `Authentication provider discovered`, failure `Discovery failed` (code 1).
4. Load integrations; resolve target (`resolveIntegration`): strip trailing `/`s; exact id match;
   else case-insensitive unique name match; multiple → `Integration name "<target>" is ambiguous: <ids comma-joined>`;
   none → `Integration not found: <target>`.
   Without target: picker of integrations having ≥1 non-`env` method, sorted: MCP-sourced
   (`metadata.source === "mcp"`) first, then priority `opencode-go`(0), `opencode`(1), `openai`(2),
   `github-copilot`(3), `anthropic`(4), `google`(5), others (6), then name, then id.
   Category label `MCP` / `Popular` (in priority map) / `Services`; connected marker `✓`.
   No choices → `No authentication integrations are available`.
   Picker UI strings: title `Select integration`, `Search:`, `No integrations found`, `…`,
   footer `↑/↓ to select • Enter: confirm • Type: to search` (`↑/↓ navigate • Enter select` when
   columns < 50), validation `Select an integration` (`ui/integration-picker.ts`).
5. Methods = integration methods except type `env`, sorted with `key` methods last. None →
   `<name> has no interactive login methods`.
   `--method`: match (`key` method: `"key"` or its label case-insensitive; others: exact id or
   label case-insensitive). Ambiguous → `Authentication method "<target>" is ambiguous`;
   none → `Authentication method not found: <target>. Available: <ids, "key" for key methods>`.
   One method → used. Several, interactive → `select` "Select login method" (key label
   default `API key`); non-interactive → `Pass --method when running without an interactive terminal`.
6. Form answers (`answerForm`, `form.ts`): `--answer key=value` items; `=` must be at index ≥1
   else `Expected --answer key=value`; unknown key → `Unknown form field: <key>`. For each field in
   order: inactive (`when` conditions: all must hold; `eq`/`neq` against prior answers, array values
   use `includes(String(value))`) → skipped (supplied answer for inactive field →
   `Form field is not active: <key>`). Supplied: `external` → `External form field requires interactive confirmation: <key>`;
   `string` raw; other types JSON-decoded (`Expected a JSON value for form field: <key>`).
   Hidden non-external fields take `default`. Otherwise prompt (requires TTY:
   `Authentication form input requires an interactive terminal`). Validation errors are
   `<key>: <message>`; text validator messages: `Required`, `Expected a number`,
   `Expected an integer`, `Must be at least N`, `Must be at most N`,
   `Must be at least N characters`, `Must be at most N characters`, `Invalid format`,
   `Expected a URL`, `Expected an email address`, `Expected a date`, `Expected a date and time`.
   Booleans not required → select Yes/No/Skip; multiselect custom option `Type another value`,
   `Select at least N` / `Select at most N`; string options custom `Type your own answer` and `Skip`.
7. Authenticate by method type:
   - `key`: password prompt (`API key input requires an interactive terminal` if no TTY), message =
     method label or `Enter your <name> API key`, `Required` validation; spinner `Saving credential...` →
     `Connected to <name>` / `Authentication failed`.
   - `external`: spinner `Saving credential...`, POST connect/external.
   - `oauth`: spinner `Starting authorization...`; POST connect/oauth `{methodID, answer, label, location}`;
     `Authorization started`; log instructions then URL; open browser if TTY. Mode `code`: prompt
     `Paste the authorization code` → spinner `Completing authorization...` → complete. Otherwise
     `Waiting for authorization...` and poll status every 500 ms until not `pending`:
     `complete` → `Connected to <name>`; `failed` → error `status.message`; else `Authorization expired`.
     A finalizer cancels the attempt (DELETE, 5 s timeout) on any exit.
   - `command`: spinner `Starting authentication command...` → `Authentication command started`,
     `Waiting for authentication command...`, poll every 500 ms, printing each newly completed
     line of the accumulating status message; result `Connected to <name>` / error /
     `Authentication expired`. Finalizer cancels.
8. `outro("Done")`.
Errors (`handlePromptErrors`, `ui/prompt.ts`): prompt cancel → `cancel("Cancelled")`, exit 130;
interrupt → `cancel("Cancelled")`; other → `log.error(message)`, `outro("Failed")`, exit 1.

### 3.7 `auth logout` / `auth switch` (`logout.ts`, `switch.ts`, `account.ts`)

- Non-interactive guards: missing target → `Pass an integration ID or name when running without an interactive terminal`;
  missing credential → `Pass a credential ID or label when running without an interactive terminal`.
- Intro `Log out of an account` / `Switch account`.
- Integration picker (when no target): integrations with ≥1 `credential` connection; none →
  `No stored credentials found`; autocomplete `Select integration` (hint = labels joined by `, `).
- Credential choice: no stored credentials → if action is log out and env connections exist:
  `<name> is authenticated through <ENV1, ENV2>; unset the environment variable to disconnect`;
  else `No stored credentials for <name>`. With target: exact id, else unique case-insensitive label;
  ambiguous `Credential label "<t>" is ambiguous. Use an ID: <ids>`; none
  `Credential not found for <name>: <t>`. Picker message `Select <name> account to log out|switch to`,
  first credential labeled `<label> (active)`, hint = id.
- Logout: spinner `Removing credential...` → `Removed account from <name>` / `Failed to remove credential`; `DELETE /api/credential/{id}`.
- Switch: spinner `Switching account...` → `Switched account for <name>` / `Failed to switch account`; `POST /api/credential/{id}/activate`.
- `outro("Done")`. Errors per `handlePromptErrors`.

### 3.8 `auth export` / `auth import`

Export (`export.ts`): optional target resolved to integration id; `GET /api/credential`, filter by
`integrationID`. If stdout is a TTY, stderr
`Warning: the output contains secrets; redirect it to a file or pipe it to auth import` + EOL.
Print `JSON.stringify(credentials, null, 2) + EOL`. Errors → message to stderr, exit 1.

Import (`import.ts`): without file and stdin is a TTY → `Pipe auth export output into stdin or pass a file to import`.
Read file or all of stdin; decode as array of `Credential.Entry`. List existing credentials; for each
entry: skip if id exists; else `POST /api/credential {id, integrationID, label, value, activate: entry.active && integration had no credentials before}`;
HTTP conflict → skipped. stderr: `Imported N credential|credentials` + (`, skipped M already present` if any skipped) + EOL. Errors → stderr message, exit 1.

### 3.9 `mcp list` (`handlers/mcp/list.ts`)

`GET /api/mcp`, sort by name (localeCompare). Empty → `No MCP servers configured`. Else
`width = max name length`; each line `<icon> <name.padEnd(width)>  <description>`:

| status | icon | description |
|---|---|---|
| `connected` | `✓` | `connected` |
| `needs_auth` | `⚠` | `needs authentication` |
| `failed` | `✗` | `failed: <error>` |
| other (e.g. `disabled`, `pending`) | `○` | status string |

### 3.10 `mcp add` (`handlers/mcp/add.ts`)

- Local command = `process.argv` after the first `--` if present, else the positional `command` values.
- Validation errors (exact): `Provide either --url <url> or a command after --, not both`;
  `Provide either --url <url> or a command after --`; `Invalid URL: <url>`;
  `--env is only valid for local MCP servers`; `--header is only valid for remote MCP servers`.
- Entry: remote `{ "type": "remote", "url", "headers"? }`, local `{ "type": "local", "command": [...], "environment"? }`.
- Target file (`resolveConfigPath(dir)`), `dir` = global config dir with `--global` else cwd; first
  existing regular file among `opencode.json`, `opencode.jsonc`, `.opencode/opencode.json`,
  `.opencode/opencode.jsonc`; default `opencode.json`.
- Edit with JSONC-preserving modify at path `["mcp","servers",<name>]`, formatting 2 spaces;
  missing file treated as `{}`. Written in place (no temp file, no trailing newline added).
- stdout `MCP server "<name>" added to <path>` + EOL.

### 3.11 `mcp auth` / `mcp logout`

`mcp auth` (`mcp/auth.ts`): needs name when non-interactive
(`Pass an MCP server name when running without an interactive terminal`). Intro
`Authenticate an MCP server`. Choices: MCP servers whose integration has an `oauth` method
(label = server name, hint `needs authentication` / `failed` / `disabled`), sorted by label.
None and no name: warn `No OAuth-capable MCP servers configured`, info
``Remote MCP servers support OAuth by default. Add one with `opencode mcp add` or in opencode.json:`` followed by

```
  "mcp": {
    "my-server": {
      "type": "remote",
      "url": "https://example.com/mcp"
    }
  }
```
then `outro("Done")`. Name not found → `MCP server not found: <name>`; no oauth method →
`MCP server "<name>" is not an OAuth-capable remote server`. If already connected: status
`needs_auth` → warn `<name> has expired credentials. Re-authenticating...`; otherwise (TTY)
confirm `<name> already has valid credentials. Re-authenticate?` — no → `outro("Cancelled")`.
Runs the OAuth flow of §3.6 with the previous active credential's label, then deletes all
previous credentials; `outro("Done")`.

`mcp logout` (`mcp/logout.ts`, `resolve.ts`): unknown server → error `MCP server not found: <name>`
(uncaught → stderr pretty error, exit 1). No integration or no stored credentials →
stdout `No stored credentials for <name>`. Else delete each credential; stdout
`Removed OAuth credentials for <name>`.

### 3.12 `plugin *`

`plugin list` (`plugin/list.ts`): server plugins from `GET /api/plugin` plus TUI plugins from the CLI
config `plugins` and locally discovered plugin dirs. Configured entries skipped when target starts
with `-`, equals `*`, ends with `.*`, or starts with `opencode.`. Table:

```
ID      VERSION  SOURCE
<id>    <ver>    <target>
```
Columns 0 and 1 padded to max width, two spaces between columns; versions that are 40/64-hex
commit hashes are shortened to 7 chars; server rows: id (`-` if none), version (`package` → version
or `-`; `local` → `local`; else `-`), source (`package` target / local path / source type).
Builtin plugins hidden unless `--builtin`. Sorted by id then source. Empty → `No plugins found`.

`plugin add` (`plugin/add.ts`): package must be installable (npm registry or git spec) else
`Plugin target must be an npm registry package or Git package specifier`; installs into the npm cache
(§4 `cache/npm`), resolves entrypoints; none → `Plugin package has no server or TUI entrypoint: <pkg>`.
Server plugin → append to `plugins` array in the global `opencode.json[c]` (resolved like §3.10 in the
global config dir; JSONC parse with trailing commas allowed; invalid →
`Invalid global configuration: <path>`, non-array `plugins` → `Invalid plugins configuration: <path>`;
written via `<path>.tmp` (mode 0600) + rename, ensuring trailing newline). Messages:
`Plugin "<pkg>" installed and added to <path>` or `Plugin "<pkg>" is already configured in <path>`.
TUI-only plugin → appended to CLI config `plugins` (`cli.json`), message
`TUI plugin "<pkg>" installed and added to <cli.json path>`.

`plugin check` (`check.ts`, `inventory.ts`): output grouped:
```
Server
  <name> <version> (current|update available|check failed)
TUI
  <name> <version> (...)
```
Empty → `No package plugins found`. Exit 1 if any item has an error. Selected target not configured →
error `Plugin is not configured: <target>`.

`plugin update`: prints `Failed to check <Server|TUI> plugin "<name>": <error>` to stderr per failed check;
no outdated → `No plugin updates available` (exit 1 if check errors). Otherwise updates concurrently:
server via `POST /api/plugin/update {targets:[target]}`, TUI via npm update; per item
`Updated <runtime> plugin "<name>"` or stderr `Failed to update <runtime> plugin "<name>": <pretty cause>` (exit 1).

`plugin remove` (`remove.ts`): removes matching entries (string equal or `{package}` equal) from the
global server config `plugins` and from CLI config `plugins`. Message
`Plugin "<pkg>" removed from <path1>[, <path2>]` or `Plugin "<pkg>" is not configured`.

### 3.13 `session list` (`handlers/session/list.ts`)

1. `GET /api/location?location[directory]=<cwd>` → `location.project.id`.
2. `GET /api/session?project=<id>&parentID=null&order=desc&limit=<max-count or 100>`.
3. `table` with zero sessions → prints nothing, exit 0.
4. `json`: `JSON.stringify(sessions.map(s => ({id, title, updated: s.time.updated, created: s.time.created, projectId: s.projectID, directory: s.location.directory})), null, 2) + EOL`.
5. `table`: per session `id \t title \t updated` where title is `title ?? "Untitled session"` with
   `\r`, `\n`, `\t` replaced by a space, and `updated` is `new Date(ms).toLocaleString()` (host locale,
   e.g. `10/12/2026, 3:04:05 PM` in en-US); lines joined by EOL plus trailing EOL.
6. Paging: if stdout is a TTY, `--max-count` not given, and format is table → pipe output into
   `less -R -S` (Windows: `cmd /c more`) with inherited stdout/stderr; if the pager cannot be
   spawned, write directly.
7. Errors → stderr `errorMessage + EOL`, exit 1.

### 3.14 `session delete`

`DELETE /api/session/{sessionID}`; stdout `Session <id> deleted` + EOL. Errors → stderr message, exit 1.

### 3.15 `session export`

- No ID and stdin not a TTY → error `Pass a session ID when running without an interactive terminal` (stderr, exit 1).
- Interactive picker (all clack output on **stderr**): intro `Export session`; sessions of the
  current location directory (`GET /api/session?directory=<dir>&parentID=null&order=desc&limit=50`);
  none → `No sessions found`, outro `Done`, exit 0. Autocomplete `Select session to export`
  (max 10 items, label = title, hint `<toLocaleString(updated)> - <last 8 chars of id>`);
  cancel → `Cancelled`, exit 130; selection → outro `Exporting session...`.
- `GET /api/experimental/session/{id}/export?sanitize=<bool>`; stdout `JSON.stringify(data, null, 2) + EOL`.

### 3.16 `session import`

- `file` starting with `http://`/`https://` → fetched (non-OK →
  `Failed to fetch session data: <statusText>`); else read file. Read failures →
  `Failed to read session data: <message>`.
- Decode as `SessionTransfer.Data` (schema; decode failure is an uncaught error, exit 1).
- Resolve location for `path.resolve(--directory ?? cwd)`.
- `POST /api/experimental/session/import` with JSON `{...encodedData, location: {directory}}`.
- 409 → stderr `Session already exists` + EOL, exit 0. Non-OK → `Failed to import session: <statusText>`.
- Success: stdout `Imported session: <id>` + EOL.

---

## 4. Global paths (`util/src/global.ts`, `util/src/global-roots.ts`)

Application name `opencode`. Roots are XDG-based **on every OS** (including macOS and Windows —
no `%APPDATA%`/`~/Library` logic):

| Base | Value |
|---|---|
| `homedir` | `os.homedir()` (`$HOME` / `%USERPROFILE%`) |
| `XDG data` | `$XDG_DATA_HOME` if non-empty, else `<home>/.local/share` |
| `XDG cache` | `$XDG_CACHE_HOME` if non-empty, else `<home>/.cache` |
| `XDG config` | `$XDG_CONFIG_HOME` if non-empty, else `<home>/.config` |
| `XDG state` | `$XDG_STATE_HOME` if non-empty, else `<home>/.local/state` |

(Empty-string env values fall back — JS `||`.) Roots are computed once at module load.

| Key | Path | Notes |
|---|---|---|
| `home` | `$OPENCODE_TEST_HOME` ?? `os.homedir()` | evaluated lazily |
| `data` | `<XDG data>/opencode` | |
| `cache` | `<XDG cache>/opencode` | |
| `config` | `$OPENCODE_CONFIG_DIR` if set (non-empty), else `<XDG config>/opencode` | override applied by the Global layer (`global.ts:79`, `cli/src/index.ts:123`, `server-process.ts:38`) |
| `state` | `<XDG state>/opencode` | |
| `tmp` | `realpath(<os.tmpdir()>/opencode)` | canonicalized after creation (e.g. `/private/var/...` on macOS) |
| `bin` | `<cache>/bin` | |
| `log` | `<data>/log` | |
| `repos` | `<data>/repos` | |

On acquisition the service creates (`mkdir -p`) `data, config, state, log, bin, repos, tmp`
(`global.ts:63-75`). Other well-known locations:

- Lock directory: `<state>/locks` (`util/src/flock.ts:19-22`); lock for key K is
  `<state>/locks/<sha1hex(K)>.lock` (a directory containing `meta.json` and `heartbeat`;
  stale after 60 s without heartbeat, acquire timeout 5 min).
- npm package cache: `<cache>/npm/<sanitized name@spec>` (git specs:
  `git-<slug>-<sha256(spec)[0..12]>`), generations as numeric subdirectories (`util/src/npm.ts:86-90,141`).
- Workerd builds root everything at `<tmpdir>/opencode/{data,cache,config,state,tmp}` (not relevant for a native port).

## 5. Database file (`cli/src/database-path.ts`)

```
filename = $OPENCODE_DB
        ?? ( channel ∈ {latest, dev, beta, next, prod}
             || $OPENCODE_DISABLE_CHANNEL_DB ∈ {"1","true"}   (exact, case-sensitive)
             ? "opencode.db"
             : "opencode-" + sanitize(channel) + ".db" )
path = filename === ":memory:" ? ":memory:" : path.resolve(<data>, filename)
```

So release builds use `<data>/opencode.db`; local dev builds `<data>/opencode-local.db`; preview
channel `foo/bar` → `opencode-foo-bar.db`. An absolute `OPENCODE_DB` is used as-is; a relative one
is resolved against the data dir. The server receives this as `database.path` (`server-process.ts:107-109`).

---

## 6. Configuration

This section specifies how opencode v2 discovers, parses, normalizes, merges, watches and reports configuration. There are two **independent** configuration systems:

1. **Location (server) configuration** – `opencode.json` / `opencode.jsonc`, loaded by the core `Config` service for each *location* (working directory) the server serves. Implemented in `packages/core/src/config.ts`, `packages/core/src/config/*`, schema in `packages/schema/src/config.ts` + `packages/schema/src/config/**`.
2. **CLI / TUI client configuration** – a single global `cli.json`, owned by the terminal client only (the background service never reads it). Implemented in `packages/cli/src/config/**`, schema re-exported from `packages/tui/src/config/index.tsx`.

All paths below are relative to the opencode checkout root. `path:N` means line N of that file. Strings in `"…"` / backticks are verbatim.

---

### 1. Location configuration: data model

#### 1.1 Entries

`Config.Service.entries()` returns an ordered list of **entries**, lowest priority first (`packages/core/src/config.ts:28-29`). An entry is a tagged union (`packages/schema/src/config.ts:117-131`):

| Entry | Fields | Meaning |
|---|---|---|
| `Document` | `type: "document"`, `path?: AbsolutePath`, `info: Info` | One parsed + normalized config document. `path` is absent for virtual sources (wellknown, `OPENCODE_CONFIG_CONTENT`). |
| `Directory` | `type: "directory"`, `path: AbsolutePath` | A config *root directory* (global config dir or a present `.opencode` dir). Domain plugins scan it for Markdown agents/commands, plugins, skills. |

There is **no global deep-merge** of documents into one object. Each domain consumer folds the entry list itself with its own rule (section 6). The helper `Config.latest(entries, key)` returns `info[key]` of the **last** `Document` whose `info[key] !== undefined` (`packages/core/src/config.ts:22-25`) – i.e. "highest-priority defined value wins, whole value replaced".

The entry list is cached in memory; `entries()` returns the cached list and only changes after a reload (`packages/core/src/config.ts:353-355`; test "loads opencode JSON and JSONC files from lowest to highest priority", `packages/core/test/config/config.test.ts:1134-1177`, asserts a file edit is not visible until reload).

The HTTP API exposes the list unchanged: `GET /api/config?location[directory]=…` → `Array<Config.Entry>` (`packages/protocol/src/groups/config.ts:9-21`; handler `packages/server/src/handlers/config.ts:9`). Description: "Return configuration documents and discovery sources for the requested location, from lowest to highest priority."

#### 1.2 Config service options

`Config.Options` (`packages/core/src/config.ts:45-52`):

| Option | Type | Effect |
|---|---|---|
| `project` | boolean? | `false` skips the upward project walk entirely (`packages/core/src/config/discovery.ts:33-36`). |
| `global` | boolean? | `false` skips the global config dir, `~/.claude` and `~/.agents`; wellknown, explicit file and content still load (comment `config.ts:47-48`). |
| `file` | string? | Explicit config file (from `OPENCODE_CONFIG`). Resolved with `path.resolve(options.file)` against the **process** cwd (`discovery.ts:65`). |
| `content` | string? | Inline JSONC (from `OPENCODE_CONFIG_CONTENT`). |

Wiring from environment (server process): `packages/cli/src/server-process.ts:115-122`:

```
config: {
  directory: process.env.OPENCODE_CONFIG_DIR,
  project: !truthy(process.env.OPENCODE_CONFIG_PROJECT_DISABLE ?? process.env.OPENCODE_DISABLE_PROJECT_CONFIG),
  file: process.env.OPENCODE_CONFIG,
  content: process.env.OPENCODE_CONFIG_CONTENT,
}
```

`truthy(v)` is `v === "1" || v?.toLowerCase() === "true"` (`server-process.ts:210-212`). `config.directory` replaces the Global `config` directory (`packages/server/src/routes.ts:123`); `project/file/content` go to `Config.configured(...)` (`routes.ts:124-130`). `global` is never set by the server; the "vanilla" instance mode uses `Config.configured({ project: false, global: false })` (`packages/core/src/instance.ts:143`).

### 2. Environment variables and directories

| Variable | Effect | Source |
|---|---|---|
| `OPENCODE_CONFIG_DIR` | Replaces the global config directory (default `$XDG_CONFIG_HOME/opencode` or `~/.config/opencode`). Also used for `cli.json`. | `packages/util/src/global.ts:79`, `packages/cli/src/index.ts:123`, `server-process.ts:38` |
| `OPENCODE_CONFIG` | Explicit extra config file path (`Options.file`). | `server-process.ts:120` |
| `OPENCODE_CONFIG_CONTENT` | Inline JSONC config text, highest priority. Source label `"OPENCODE_CONFIG_CONTENT"`. | `server-process.ts:121`, `config.ts:206-218` |
| `OPENCODE_CONFIG_PROJECT_DISABLE` (preferred) / `OPENCODE_DISABLE_PROJECT_CONFIG` (fallback) | Truthy → `project: false` (also disables project AGENTS.md discovery). | `server-process.ts:117-119`, `routes.ts:131` |
| `OPENCODE_FILEWATCHER_DISABLE` / `OPENCODE_DISABLE_FILEWATCHER` | Truthy → filesystem watcher disabled (no automatic config reload). | `server-process.ts:126` |
| `OPENCODE_CLI_CONFIG_CONTENT` | Inline JSONC deep-merged over `cli.json` (client only). | `packages/cli/src/config/config.ts:32-34` |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` | Roots of global dirs on **all** OSes (no macOS/Windows special case). | `packages/util/src/global-roots.ts:4-18` |
| `OPENCODE_TEST_HOME` | Overrides `Global.home` (used for `~/.claude`, `~/.agents`). | `global.ts:16-18` |

Global directories (`packages/util/src/global-roots.ts:5-18`, `global.ts:12-27`): `config = (XDG_CONFIG_HOME || ~/.config)/opencode`, `state = (XDG_STATE_HOME || ~/.local/state)/opencode`, `data = (XDG_DATA_HOME || ~/.local/share)/opencode`, `cache = (XDG_CACHE_HOME || ~/.cache)/opencode`. All are `mkdir -p`'d at startup (`global.ts:66-72`).

**Managed policy locations per OS: none.** There is no `/etc/opencode`, `/Library/Application Support`, or `ProgramData` managed config file. "Managed policy" (`packages/core/src/managed-policy.ts`) is an in-memory statement list pushed by the OpenCode Console provider plugin (`packages/core/src/plugin/provider/opencode.ts:245-246`: `managed.set({ statements: next.config?.experimental?.policies ?? [], organization: next.organization })`). See 6.10.

### 3. Discovery

#### 3.1 File names

`ConfigDiscovery.names = ["opencode.json", "opencode.jsonc"]` (`packages/core/src/config/discovery.ts:11`). Within one directory `opencode.json` is loaded first and `opencode.jsonc` second, so **`.jsonc` has higher priority** when both exist (`config.ts:185-191`; test `config.test.ts:1134`). `config.json` is **not** loaded by `Config` (test "does not load legacy config.json files", `config.test.ts:1245-1262`). (Exception: the CLI updater still reads `config.json` for the update policy, see 6.12.)

#### 3.2 Algorithm (`ConfigDiscovery.discover`, `discovery.ts:23-84`)

Inputs: `global.config` (G), `global.home` (H), `location.directory` (D, the request's working directory), options.

1. `globalRoots = [resolve(G), resolve(H/.claude), resolve(H/.agents)]` where `resolve` = `path.resolve` then `realpath` (falls back to the unresolved path when NotFound) (`discovery.ts:27-32`; `packages/util/src/fs-util.ts` `resolve`).
2. Directory walk (`discovery.ts:33-36`): if `resolve(D) === globalRoots[0]` **or** `options.project === false` → no walk. Otherwise `fs.up({ targets: ["."], start: D })`: yields `D, dirname(D), …, /` (filesystem root; **no stop at the git root or home**).
3. For each walked directory `dir` (nearest first) produce candidates, in this order: `dir/.claude`, `dir/.agents`, `dir/.opencode`, `dir/opencode.jsonc`, `dir/opencode.json` (`discovery.ts:41`: `[".claude", ".agents", ".opencode", ...names.toReversed()]`). Each candidate is also realpath-resolved (parent resolved too, so symlinked roots are honored).
4. Filter (`discovery.ts:55-61`):
   - global enabled: drop candidates whose resolved path equals a global root or equals `resolve(G/opencode.json)` / `resolve(G/opencode.jsonc)` ("Global sources must not re-enter through the project walk").
   - global disabled: drop candidates whose resolved path equals or is inside any global root.
5. Result (`Sources`, `discovery.ts:63-83`), every list ordered **farthest → nearest** (the walk list is reversed):
   - `global`: `G` if global enabled.
   - `explicit`: `path.resolve(options.file)` if set.
   - `direct`: all `opencode.json[c]` candidates (existing or not) – farthest first, and within a directory `opencode.json` before `opencode.jsonc`.
   - `project`: every `.opencode` candidate with `present = isDir(path)`.
   - `claude`: `[H/.claude (if global enabled), …dir/.claude farthest→nearest]`, de-duplicated.
   - `agents`: same for `.agents`.
   `claude`/`agents` are "compatibility roots" (only `skills/` inside them is used, see 5.4); `compatibility()` filters them to existing directories (`config.ts:356-360`).

#### 3.3 Load order / precedence (`Config.load`, `config.ts:194-238`)

Final entry list, lowest → highest priority (`config.ts:230-237`):

| # | Source | Entries |
|---|---|---|
| 1 | Wellknown (remote org) configs | `Document` (no path) per resolved config |
| 2 | Global dir `G` | `G/opencode.json`, `G/opencode.jsonc`, then `Directory(G)` |
| 3 | Explicit `OPENCODE_CONFIG` file | `Document` |
| 4 | Direct project files | every existing `opencode.json[c]` from farthest ancestor to `D` |
| 5 | Present `.opencode` dirs, farthest → nearest | per dir: `opencode.json`, `opencode.jsonc`, `Directory(dir)` |
| 6 | `OPENCODE_CONFIG_CONTENT` | `Document` (no path); variable `{file:}` base dir = `location.directory` |

Consequences (documented in `services/www/src/docs/content/config.mdx` "Locations"): every `.opencode/opencode.json[c]` outranks every direct `opencode.json[c]`, regardless of depth. Explicit file ranks **below** project files (comment `config.ts:220-221`). Verified by test "loads global and ancestor configuration across the project boundary" (`config.test.ts:1699-1787`): order `global, G-Directory, outside, root, parent, directory, root-dot, root/.opencode Directory, directory-dot, directory/.opencode Directory`.

A missing or unreadable (NotFound / PermissionDenied) file is silently skipped (`readFileStringSafe`, `packages/util/src/fs-util.ts:73-78`; `config.ts:137-138`). Any other read error, and any `{file:}` substitution error, is a **defect** (`Effect.orDie`, `config.ts:196, 203, 216, 222, 227`) that fails the whole location load (see 9.3).

#### 3.4 Wellknown (remote organization) config

Registered origins are stored in KV key `"wellknown:sources"` (`packages/core/src/wellknown.ts:101`). For each origin the manifest is fetched from `` `${origin.replace(/\/+$/, "")}/.well-known/opencode` `` (`wellknown.ts:66`) with 15 s timeout. Manifest: `{ auth?: { command: string[], env: string }, config?: Record<string, Json> | null, remote_config?: { url: string, headers?: Record<string,string> } }` (`wellknown.ts:12-31`). Resolved configs = `[manifest.config?] ++ [remote JSON (or its .config property when that is a record)]`; `{env:NAME}` is substituted in `remote_config.url`/headers using the credential variables then `process.env` (`wellknown.ts:80-100`).

In `Config` (`config.ts:145-183`): the latest credential for the integration must be a `key` credential; it is exposed as env var `manifest.auth.env`. If there is no usable credential (`"No usable wellknown credential"`) or resolution fails, it logs `"failed to load wellknown config"` and falls back to `wellknown.cached(origin)` (last successful result kept until the source is removed). Each config is `JSON.stringify`'d, run through variable substitution (virtual source, `dir = origin`, extra env = credential vars) and `parseInfo` with source label = origin. Discovery failures log `"failed to discover wellknown config"` and yield `[]`.

### 4. Parsing pipeline (per document)

`loadFile(filepath)` (`config.ts:136-143`): read text → `ConfigVariable.substitute` → `parseInfo(text, source)` → `Document`.

#### 4.1 Variable substitution (`packages/core/src/config/variable.ts`)

Applied to the **raw text before JSONC parsing**, so it works anywhere (keys, values, comments).

**`{env:VAR}`** (`variable.ts:28-31`):
- Regex `/\{env:([^}]+)\}/g` (VAR = one or more non-`}` chars, not trimmed).
- Replacement: `(input.env?.[VAR] ?? process.env[VAR]) || ""` – missing **or empty** → empty string. No error, no default syntax.
- Value inserted **raw, not JSON-escaped** (a value containing `"` or newline can break the JSON → document rejected).
- No escape mechanism for a literal `{env:…}`.
- Env substitution happens first; `{file:}` only runs if the result contains `"{file:"` (`variable.ts:32`).

**`{file:PATH}`** (`variable.ts:36-84`):
- Regex `/\{file:[^}]+\}/g`.
- Comment skip: if the text from the start of the token's line up to the token, after `trimStart()`, starts with `//`, the token is left verbatim (`variable.ts:48-54`). Only whole-line `//` comments; trailing `//` comments after code and `/* */` blocks are still substituted.
- Path: `~/` prefix → `path.join(os.homedir(), rest)` (note: `os.homedir()`, not `OPENCODE_TEST_HOME`); absolute → as is; relative → `path.resolve(configDir, PATH)` where `configDir = dirname(config file)` for file sources, `input.dir` for virtual sources (`location.directory` for `OPENCODE_CONFIG_CONTENT`; the origin URL string for wellknown) (`variable.ts:38, 56-58`).
- Content: file read as UTF-8, `.trim()`ed, then `JSON.stringify(content).slice(1, -1)` – i.e. JSON-string-escaped **without** surrounding quotes, so the token must sit inside a JSON string (`variable.ts:79`). Test: file `'file\n"token"\n'` → header value `'file\n"token"'` (`config.test.ts:1179-1243`).
- Missing file: fails with `ConfigInvalidError` (`packages/core/src/v1/config/error.ts:14-18`) `{ path: <config file path or virtual source label>, message }` where message is verbatim:
  - NotFound: `` `bad file reference: "${token}" ${resolvedPath} does not exist` ``
  - other errors: `` `bad file reference: "${token}"` `` (`variable.ts:63-71`).
  (A `missing: "empty"` mode exists in the API but is never used by `Config`.)

#### 4.2 JSONC parsing (`parseInfo`, `config.ts:104-134`)

- `jsonc-parser` `parse(text, errors, { allowTrailingComma: true })`; comments (`//`, `/* */`) allowed; trailing commas allowed.
- **Any** parse error rejects the whole document (no partial recovery, no line/column reported). Logged as warning (see 9.1) with `path: "$"`, `kind: "invalid"`, `action: "rejected malformed JSON or JSONC document"`; the document is dropped and other documents still load (test "ignores an invalid file while loading valid config values", `config.test.ts:1672-1697`).
- Then `ConfigNormalize.normalize(input)` (section 7); every diagnostic is logged; if `rejected` → dropped.
- Then final decode of the normalized object with `Schema.decodeUnknownOption(Info, { errors: "all", onExcessProperty: "ignore" })`; failure logs `action: "rejected canonical configuration after final validation"` and drops the document (`config.ts:126-133`).
- Unknown top-level keys are silently dropped by normalization (no diagnostic) unless listed as "unsupported" (7.2).
- `$schema` is accepted as metadata, never written back (test `config.test.ts:1264-1286`).

### 5. Markdown- and directory-based configuration

These are produced by domain plugins from `Directory` entries (global dir `G` and each present `.opencode` dir), **not** from walked plain directories. Markdown parsing: `ConfigMarkdown.parse` (`packages/core/src/config/markdown.ts:4-14`) uses `gray-matter` with an explicit `{}` options object (bypasses gray-matter's cache, upstream gray-matter#166). On YAML failure it retries with `sanitize(content)` (`markdown.ts:26-40`): inside the leading `---\n…\n---` block, each non-indented, non-comment, non-empty line matching `^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$` whose trimmed value is non-empty, not `>`/`|`, not quoted, and **contains `:`** is rewritten to a block scalar `key: |-` + `  value`. `parseOption` returns `undefined` if both attempts throw (file skipped silently).

#### 5.1 Agents (`packages/core/src/config/plugin/agent.ts`)

- Globs per Directory (scanned with `dot: true, symlink: true`, sorted lexicographically per glob) (`agent.ts:21-24, 164-175`):
  - `{agent,agents}/**/*.md` → `primary: false` (recursive)
  - `{mode,modes}/*.md` → `primary: true` (non-recursive) – forces `mode: "primary"`.
- Agent name = path relative to the Directory, `\` → `/`, strip leading `agent/|agents/|mode/|modes/`, strip `.md` (`agent.ts:180-184`). Nested files give names like `team/reviewer`.
- Body (`content.trim()`) becomes `system` (v2) or `prompt` (v1).
- Format detection: if **any** frontmatter key is not in `{"variant", ...ConfigAgent.Info fields}` (= `model, request, system, description, mode, hidden, color, steps, disabled, permissions, variant`) the file is treated as **legacy v1** (`agent.ts:32, 186`), decoded with `ConfigAgentV1.Info` (with `name` injected) and migrated with `ConfigMigrateV1.migrateAgent` (7.5). Otherwise decoded as `ConfigAgent.Info`; if `model` is a string without `#` and `variant` is a string matching `/^[^#]+$/`, model becomes `` `${model}#${variant}` `` (`agent.ts:189-196`).
- Decode failures skip the file silently. Each file yields a `Document { path: file, info: { agents: { [name]: agent } } }` positioned at the Directory's slot in the entry order (`agent.ts:205-211`).
- Reload: config `changes()` filtered to paths under `<Directory>/{agent,agents,mode,modes}` plus `config.updated` events, debounced 100 ms (`agent.ts:64-81, 133-139`).

#### 5.2 Commands (`packages/core/src/config/plugin/command.ts`)

- Glob `{command,commands}/**/*.md` per Directory, sorted (`command.ts:156-172`).
- Name = relative path, strip `command/|commands/`, strip `.md` (`command.ts:180-185`).
- Frontmatter decoded with `ConfigCommand.Info` and `template = content.trim()` (`command.ts:177`). Invalid → skipped.
- Directory commands occupy the Directory's slot; later definitions of the same name replace earlier ones entirely (`editor.set(name, …)`, `packages/core/src/command.ts:61`).
- Template evaluation (`command.ts:189-250`): `$1..$N` positional args (last placeholder swallows the rest), `$ARGUMENTS` = raw input; if neither appears and input non-empty, input is appended after a blank line; `` !`cmd` `` runs a shell (concurrency 2) and splices combined output; failure message `` `Shell interpolation failed for ${JSON.stringify(source)}: ${error.message}` ``. Args regex `/(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)/gi`.
- `subagent ?? subtask` (or agent mode `subagent`) runs the command in a child session with prefix text `"You are a subagent spawned by another session."`.

#### 5.3 Plugins (`packages/core/src/plugin/source-directory.ts`, `packages/core/src/config/plugin/source.ts`)

- Per Directory, children of `plugin/` and `plugins/` (sorted by full path): files ending `.ts`/`.js`, directories, and symlinks to either (`source-directory.ts:7-33`). Each becomes an `add` operation.
- Configured `plugins` entries (resolution rules in 8.13) are applied **after** auto-discovered ones so `"-name"` can remove discovered packages (`source.ts:158-159`).

#### 5.4 Skills (`packages/core/src/config/plugin/skill.ts`, `compatibility.ts`, `skill-file.ts`)

- For each Directory: `<dir>/skill` and `<dir>/skills` (`skill.ts:85-88`). Plus `skills` config items (resolution rules in 8.13). Plus compatibility roots `<claude|agents root>/skills` (`compatibility.ts:39-40`).
- Files: glob `{*.md,**/SKILL.md}` (`skill.ts:121`). Skill id = file basename without `.md` for top-level `*.md` (not `SKILL.md`), else the parent directory name. Frontmatter: `name?`, `description?`, `metadata?`, `"disable-model-invocation"?`; `metadata["opencode/autoinvoke"]` (bool-ish: `true/yes/on/1`, `false/no/off/0`, 1/0) overrides `disable-model-invocation` (`skill-file.ts:9-62`).

### 6. Merge semantics per key (how documents combine)

Documents are folded in entry order (low → high). Rules per top-level key:

| Key | Rule | Source |
|---|---|---|
| `shell` | latest defined wins | `config/plugin/shell.ts:16` |
| `model` | latest defined wins (whole selection) | `config/plugin/provider.ts:91` |
| `default_agent` | latest defined wins (over JSON docs + Markdown agent docs) | `config/plugin/agent.ts:88` |
| `snapshots` | latest defined wins | `config/plugin/snapshot.ts:16` |
| `tool_output` | latest defined **object** wins (not field-merged) | `config/plugin/tool-output.ts:16` |
| `websearch` | latest defined wins; `false` disables default | `config/plugin/websearch.ts:14-16` |
| `warming` | latest defined wins; `true` → defaults; object → `{...defaults, ...obj}` | `packages/core/src/plugin/warming.ts:20-22` |
| `experimental.subagent_depth` / `portable_shell_scanner` | latest document that has `experimental` (whole object) – then read the field (`?? 1` / `?? false`) | `tool/plugin/subagent.ts:129`, `tool/plugin/shell.ts:127` |
| `compaction` | each document's defined leaves applied in order (field-wise overlay of `auto`, `buffer`, `keep.tokens`) | `config/plugin/compaction.ts:16-22` |
| `media.image` | field-wise overlay in order | `config/plugin/image.ts:16-25` |
| `worktree` | each document re-configures `directory` (last wins) | `config/plugin/worktree.ts:22-31` |
| `watcher.ignore` | **concatenated** across documents | `config/plugin/location-watcher.ts:16-19` |
| `formatter` | fold: a boolean **resets** everything before it; an object merges per formatter name (`{...prev, ...item, environment: {...prev.env, ...item.env}}`) | `config/plugin/formatter.ts:62-76` |
| `mcp.timeout` | `Object.assign` of all documents' timeout objects in order | `config/plugin/mcp.ts:42-45` |
| `mcp.servers` | per name, last document's server **replaces** whole entry; final server timeout = `{...globalTimeout, ...server.timeout}`; servers already registered by other plugins are not overwritten | `config/plugin/mcp.ts:46-55` |
| `agents` | per agent, per field overlay in document order (`model`, `system`, `description`, `mode`, `hidden`, `color`, `steps` replace; `request.headers/body` `Object.assign`-merged; `permissions` **appended**); `disabled: true` removes the agent | `config/plugin/agent.ts:94-126` |
| `permissions` (top-level) | concatenated across all documents in order, appended to every agent before agent-specific rules (rules are last-match-wins) | `config/plugin/agent.ts:84-92, 104-105` |
| `commands` | per name, last replaces | `config/plugin/command.ts:73-77`, `command.ts:61` |
| `references` | per name, last replaces; insertion order = first appearance | `config/plugin/reference.ts:23-48`, `reference.ts:99`; test `reload.test.ts:237-270` |
| `providers` | folded per document; per-provider fields overlay; `settings`/`body` deep-merged (`mergeOverlay`: records recurse, other values replace); `headers` merged case-insensitively (`mergeHeaders`); models per field (see 8.21) | `config/plugin/provider.ts:54-162`, `provider.ts:186-219` |
| `plugins` | concatenated operations from all documents in order (after directory discovery) | `config/plugin/source.ts:134-147` |
| `skills` | union (deduplicated) of all items | `config/plugin/skill.ts:77-103` |
| `experimental.policies` | see 6.10 | `managed-policy.ts` |
| `instructions`, `lsp`, `share`, `enterprise`, `username`, `update`, `$schema` | accepted and validated; **no core consumer** (docs: instructions "accepted but does not load its entries"; share "not supported yet"; `update` read by CLI updater only, 6.12; `lsp` only read by app UI) | `services/www/src/docs/content/config.mdx` |

#### 6.10 Policies and managed policy

`ManagedPolicy.statements(entries, org)` (`packages/core/src/managed-policy.ts:50-64`): authored documents are **reversed** (so global outranks project), their `experimental.policies` flattened and tagged `message: "Blocked by configuration policy"`; then organization statements appended with message `` `Blocked by ${organization}'s policy` `` or `"Blocked by your organization's policy"`. Evaluation is last-match-wins with `Wildcard.match` (`*`→`.*`, `?`→`.`, trailing `" *"` also matches nothing; case-insensitive on win32) (`managed-policy.ts:66-75`, `packages/core/src/util/wildcard.ts:3-14`). Default `"allow"`.

Applied in `packages/core/src/config/plugin/policy.ts`: `provider.use` vs provider id (deny removes provider); `integration.use` vs `` `mcp:${name}` `` and `` `skill:${id}` ``; `tool.use` vs `` `${action}:${resource}` `` – a deny turns the permission into `deny` with `event.message = policy.message`; policies never grant.

#### 6.12 `update` policy (CLI updater)

`packages/cli/src/services/updater.ts:110-125, 150-158`: reads `G/config.json`, `G/opencode.json`, `G/opencode.jsonc` (in that order; last decodable wins; default `"notify"`). JSONC parse; if root has `update` key → valid only if `"disable"|"notify"|"auto"` (otherwise ignored, **no** fallback to `autoupdate`); else `autoupdate: false→"disable"`, `"notify"→"notify"`, `true→"auto"`. Project-level values ignored.

### 7. Normalization and v1 → v2 migration (`packages/core/src/config/normalize.ts`)

Every document (v1, v2 or mixed) goes through `normalize(input)`. Output `{ type: "normalized", encoded, diagnostics }` or `{ type: "rejected", diagnostics }` (`normalize.ts:36-42`). Decoding uses `{ errors: "all", onExcessProperty: "ignore" }` (`normalize.ts:44`); native values are canonicalized by decode-then-encode (`decodeEncoded`, `normalize.ts:687-702`) and `undefined` properties stripped (`plain`, `normalize.ts:715-721`). Malformed recognized values are skipped individually (per map entry / per list item) with a diagnostic; the rest of the document survives.

#### 7.1 Diagnostics (verbatim messages)

| kind | message | emitted by |
|---|---|---|
| `invalid` | `"rejected configuration because its root is not an object"` (path `["$"]`, result `rejected`) | `normalize.ts:58-64` |
| `invalid` | `"skipped malformed recognized value"` | `invalid()`, `normalize.ts:780-782` |
| `conflict` | `"retained native value over legacy value"` | `conflict()`, `normalize.ts:784-786` |
| `unsupported` | `"omitted unsupported legacy setting"` | `unsupportedIfPresent()`, `normalize.ts:775-778` |
| `unsupported` | `"omitted unsupported legacy model reference"` | `normalize.ts:141-146, 602-607` |
| `unsupported` | `"omitted unsupported legacy model variant"` | `normalize.ts:608-617` |
| `unsupported` | `"omitted enabled-only legacy MCP entry"` | `normalize.ts:269-271` |

Diagnostics never contain values (tests `normalization.test.ts:224-230, 506-537`). Invalid v1 permission entries use the **index** as path segment, not the resource key (`normalize.ts:520`).

`prefer(legacy, native)`: native wins; conflict only if both defined and not deep-equal (`normalize.ts:769-773`). `mergeMaps(legacy, native)`: union, native entry wins per name, conflict per differing name (`normalize.ts:735-747`). `overlay`: later value wins, conflict if differs (`normalize.ts:758-767`).

#### 7.2 Unsupported (dropped with diagnostic)

- Top-level: `logLevel`, `server`, `subagent_depth`, `layout` (`normalize.ts:45`).
- `experimental.*`: `disable_paste_summary`, `batch_tool`, `openTelemetry`, `primary_tools`, `continue_loop_on_deny` (`normalize.ts:46-52`).
- `provider.<id>.*`: `id`, `whitelist`, `blacklist` (`normalize.ts:53`).
- `provider.<id>.models.<m>.*`: `release_date`, `attachment`, `reasoning`, `temperature`, `experimental`; `status` unless `"deprecated"`; `interleaved` when boolean (`normalize.ts:54, 570-582`).
- `agent.<n>.name` / `mode.<n>.name` (`normalize.ts:584-591`).
- `compaction.tail_turns`, `compaction.prune` (`normalize.ts:332-333`).

#### 7.3 Top-level key mapping (v1 → v2)

Order of processing as in `normalize()` (`normalize.ts:57-230`):

| v1 key | v2 key | Mapping | Line |
|---|---|---|---|
| `snapshot: boolean` | `snapshots` | copy; native `snapshots` overlays (conflict if differs) | 70-72, 88, 205-224 |
| `autoupdate: true \| false \| "notify"` | `update` | `false→"disable"`, `"notify"→"notify"`, `true→"auto"`; native `update` preferred | 73-98 |
| `autoshare: boolean` | `share` | `true → "auto"`; `false` → nothing; native `share` overlays | 79-83, 99 |
| `attachment` | `media` | decoded as `ConfigMedia.Info`; native `media` overlays | 84-87 |
| `reference` | `references` | map merge (native wins per name) | 101-116 |
| `command` | `commands` | each entry: `ConfigCommandV1.Info` → `migrateCommand` (7.6); native wins per name | 118-131 |
| `agent` | `agents` | each: `ConfigAgentV1.Info` → `migrateAgent` (7.5) | 133-136 |
| `small_model: string` | `agents.title.model` | `modelSelection(small_model)`; existing `agent.title` fields win (`{model, ...legacyAgents.title}`); invalid → unsupported diagnostic | 137-151 |
| `mode` | `agents` | as `agent` but forces `mode: "primary"`; precedence `agent < mode < agents` | 152-166 |
| `provider` | `providers` | 7.7; native wins per id | 168-177 |
| `tools: {name: bool}` | `permissions` | `{action: normalizeAction(name), resource: "*", effect: bool ? "allow" : "deny"}` | 179, 487-498 |
| `permission` | `permissions` | 7.4 | 180, 500-526 |
| — | `permissions` | final = `[...toolRules, ...permissionRules, ...nativePermissions]`; emitted if non-empty or native array present | 181-183 |
| `plugin: (string \| [string, opts])[]` | `plugins` | tuple → `{ package, options }`; `[...legacy, ...native]` | 185-190 |
| `skills: string[] \| {paths?, urls?}` | `skills` | object → `[...paths, ...urls]` | 232-246 |
| `mcp` (v1 map) | `mcp.servers` | 7.8 | 248-298 |
| `experimental.mcp_timeout` | `mcp.timeout.{catalog,execution}` | same PositiveInt for both | 252-263 |
| `compaction` | `compaction` | `auto` kept; `preserve_recent_tokens → keep.tokens`; `reserved → buffer`; native preferred | 322-379 |
| `enabled_providers: string[]` | `experimental.policies` | if present and (empty or ≥1 valid item): `{provider.use, "*", deny}` then `{provider.use, providerID(x), allow}` per item | 388-398 |
| `disabled_providers: string[]` | `experimental.policies` | `{provider.use, providerID(x), deny}` per item, after enabled ones | 399-406 |
| `experimental.policies` | `experimental.policies` | appended after generated ones | 433-445 |
| `experimental.portable_shell_scanner`, `experimental.subagent_depth` | same | copied | 415-432 |
| `watcher` | `watcher` | `{ ignore }` items decoded individually | 450-458 |
| `formatter` | `formatter` | boolean kept; map entries decoded individually; all-invalid non-empty map omitted; `{}` kept | 460-474 |
| `lsp` | `lsp` | same rule as formatter | 476-485 |
| `model` | `model` | `ConfigModel.Selection`; `providerID` renamed via `providerID()` | 200-203 |
| `$schema, shell, default_agent, share, enterprise, username, snapshots, media, tool_output, websearch, worktree, warming` | same | decoded/encoded as-is, overlaid | 205-224 |
| `instructions` | `instructions` | string items decoded individually | 226-227 |

Explicit empty values are preserved (`false`, `0`, `[]`, `{}`) (test `normalization.test.ts:634`).

`providerID()` renames (`packages/core/src/v1/config/migrate.ts:163-167`): `"azure-cognitive-services" → "azure"`, `"google-vertex-anthropic" → "google-vertex"`, others unchanged.

`normalizeAction()` (`migrate.ts:28-33`): `write|patch → "edit"`, `task → "subagent"`, `bash → "shell"`, others unchanged.

`modelSelection(input, variant?)` (`migrate.ts:69-77`): returns `undefined` unless input matches `/^[^/#]+\/[^#]+$/`; splits at first `/` → `{ providerID: providerID(left), model: right, variant? }`; variant kept only if non-empty and without `#`.

#### 7.4 v1 permission format (`packages/core/src/v1/config/permission.ts`)

`Action = "ask" | "allow" | "deny"`. `Info = Action | object`; string `s` ≡ `{ "*": s }`. Object values: `Action` or `{ [resource]: Action }`. Object key order is preserved (decoded via `Object.entries`) because rules are last-match-wins. Keys `question`, `websearch`, `doom_loop` must map to a plain action; error text `"question, websearch, and doom_loop to be ask, allow, or deny"` (`permission.ts:27-40`). Arrays rejected.

Migration (`normalize.ts:500-526`): top-level string `e` → `[{action:"*", resource:"*", effect:e}]`; `key: e` → `{normalizeAction(key), "*", e}`; `key: {res: e,…}` → one rule per entry in order.

#### 7.5 v1 agent (`packages/core/src/v1/config/agent.ts`, `migrate.ts:35-57`)

v1 fields: `model?, variant?, temperature?: finite, top_p?: finite, prompt?, tools?: {name: bool}` (deprecated), `disable?, description?, mode?: "subagent"|"primary"|"all", hidden?, options?: record, color?: "#RRGGBB" | "primary"|"secondary"|"accent"|"success"|"warning"|"error"|"info", steps?: PositiveInt, maxSteps?: PositiveInt` (deprecated), `permission?` + any extra keys. Decode-time normalization (`agent.ts:45-61`): unknown keys (not in fields ∪ `name`) are moved into `options`; `tools` entries become permission entries (`write|patch → edit`, true→allow, false→deny) merged with `permission` entries via a `Map` (first position kept, later value wins); `steps = steps ?? maxSteps`.

`migrateAgent` → v2:

| v1 | v2 |
|---|---|
| `model` + `variant` | `model = modelSelection(model, variant)` |
| `options` ∪ `{temperature}` ∪ `{top_p}` | `request.body` (only if non-empty) |
| `prompt` | `system` |
| `description`, `mode`, `hidden`, `steps` | same |
| `color` | kept if starts with `#`, else `"#aaaaaa"` |
| `disable` | `disabled` |
| `permission` (entries) | `permissions` rules (with `normalizeAction`), omitted if empty |

#### 7.6 v1 command (`packages/core/src/v1/config/command.ts`, `migrate.ts:59-67`)

`{ template (required), description?, agent?, model?, variant?, subtask? }` → `{ template, description, agent, model: modelSelection(model, variant), subagent: subtask }`.

#### 7.7 v1 provider (`packages/core/src/v1/config/provider.ts`, `migrate.ts:106-241`, `provider-options.ts`)

Pre-checks per provider (`normalize.ts:528-568`): unsupported diagnostics; if `options.headers` present and not a plain object of strings, or `options.body` present and not a plain object → `invalid` and **only that provider** skipped. Old-ID entries are dropped when the current ID is also configured (`normalize.ts:549-554`).

Standard provider mapping (`migrateStandardProvider`):

| v1 | v2 |
|---|---|
| `name`, `env` | same |
| `npm` | `package = "aisdk:" + npm` (`Provider.aisdk`, prefix `"aisdk:"`, `packages/core/src/provider.ts:24-26`) |
| `options` minus `headers`/`body` | `settings` (incl. `apiKey`, `baseURL`, `enterpriseUrl`, `setCacheKey`, `timeout`, `headerTimeout`, `chunkTimeout`, any extra) |
| `api` | `settings.baseURL` (overrides) |
| `options.headers` (string values only) | `headers` |
| `options.body` | `body` |
| `models` | `models` mapped by `migrateModel` |

Special cases: `azure-cognitive-services` – drop env `AZURE_COGNITIVE_SERVICES_RESOURCE_NAME`; if `npm === "@ai-sdk/openai-compatible"` and no `api` → `settings.baseURL = "https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai"` (literal `${…}`). `google-vertex-anthropic` – provider `package` removed; each model without a package gets `package = migrated.package ?? "aisdk:@ai-sdk/google-vertex/anthropic"`.

`migrateModel` (`migrate.ts:169-241`):

| v1 model field | v2 |
|---|---|
| `id` | `modelID` |
| `family`, `name`, `headers` | same |
| `interleaved: string` / `{field}` | `compatibility.reasoningField` (boolean → nothing) |
| `options.thinking.blockBinding === false` or `options.reasoningConfig.blockBinding === false` | `compatibility.supportsThinkingBlockBinding = false`; the `blockBinding` key removed (parent removed if empty) |
| `options` | `settings` |
| `provider.npm` | `package = "aisdk:"+npm` |
| `provider.api` | `settings.baseURL` |
| `tool_call`, `modalities.input/output` | `capabilities { tools, input, output }` (defaults `true`, `["text","image"]`, `["text"]`) only if any present |
| `variants: {id: opts}` | `variants: [{ id, settings: opts }]` |
| `cost {input, output, cache_read, cache_write, context_over_200k}` | `cost: [{input, output, cache:{read, write}}, {tier:{type:"context", size:200000}, …}?]` |
| `status: "deprecated"` | `disabled: true` |
| `limit {context, input?, output}` | integers via `Math.trunc` clamped to safe-int range |

#### 7.8 v1 MCP (`packages/core/src/v1/config/mcp.ts`, `normalize.ts:248-320`, `migrate.ts:79-104`)

For each `mcp.<name>`:
- `{enabled: bool}` without `type` → dropped, `"omitted enabled-only legacy MCP entry"`.
- name `"servers"` (value not a direct server with `type: local|remote`) → v2 `servers` map.
- name `"timeout"` (same condition) → v2 timeout object; leaves `startup|catalog|execution` decoded individually; a non-empty object with none of them → invalid.
- otherwise v1 server: Local `{type:"local", command: string[], cwd?, environment?, enabled?, timeout?}`, Remote `{type:"remote", url, enabled?, headers?, oauth?: {clientId?, clientSecret?, scope?, callbackPort? (1..65535), redirectUri?} | false, timeout?}`. Mapping: `enabled → disabled = !enabled`; `timeout → {catalog: t, execution: t}`; oauth camelCase → `client_id, client_secret, scope, callback_port, redirect_uri`; `oauth: false` preserved.
- Legacy and native servers merged (native wins per name). Output `mcp = { timeout?, servers? }`; `{}` only when input `mcp` was `{}`.

### 8. Full v2 schema (`Config.Info`, `packages/schema/src/config.ts:25-110`)

All fields optional. `optional(x)` accepts absent key; `undefined` values are dropped on encode (`packages/schema/src/schema.ts:18-24`). `PositiveInt` = integer > 0; `NonNegativeInt` = integer ≥ 0 (`schema.ts:3-4`). Excess properties are ignored everywhere.

#### 8.0 Top level

| Key | Type | Default (runtime) | Description (verbatim annotation) |
|---|---|---|---|
| `$schema` | string | – | "JSON schema reference for configuration validation". Docs use `"https://opencode.ai/config.json"`. |
| `shell` | string | auto-selected | "Default shell to use for terminal and shell tool execution" |
| `model` | ModelSelection (8.1) | – | "Default model to use when no session or agent model is selected" |
| `default_agent` | string | – | "Default primary agent to use when no session agent is selected" |
| `update` | `"disable" \| "notify" \| "auto"` | `"notify"` | "Disable updates, notify when one is available, or install updates automatically" |
| `share` | `"manual" \| "auto" \| "disabled"` | – | "Control whether sessions may be shared manually, automatically, or not at all" |
| `enterprise` | `{ url?: string }` | – | "Enterprise sharing service configuration" |
| `username` | string | – | "Username displayed in conversations and used for telemetry identity" |
| `permissions` | `Rule[]` (8.2) | `[]` | "Ordered tool permission rules applied to agent tool use" |
| `agents` | `Record<string, Agent>` (8.3) | – | "Named built-in agent overrides and custom agent definitions" |
| `snapshots` | boolean | – | "Enable snapshots used for undo and revert behavior" |
| `watcher` | `{ ignore?: string[] }` | – | "Filesystem watcher configuration" |
| `formatter` | `boolean \| Record<string, FormatterEntry>` (8.4) | – | "Enable built-in formatters or configure formatter overrides" |
| `lsp` | `boolean \| Record<string, LspEntry>` (8.5) | – | "Enable built-in language servers or configure server overrides" |
| `media` | `{ image?: MediaImage }` (8.6) | – | "Media processing configuration" |
| `tool_output` | `{ max_lines?: PositiveInt, max_bytes?: PositiveInt }` | `2000` / `51200` (`packages/core/src/tool-output.ts:13-14`) | "Tool output truncation thresholds" |
| `mcp` | `{ timeout?: McpTimeout, servers?: Record<string, McpServer> }` (8.7) | – | "MCP server configuration" |
| `compaction` | Compaction (8.8) | – | "Conversation compaction behavior" |
| `skills` | string[] | – | "Additional paths or URLs to discover skills from" |
| `commands` | `Record<string, Command>` (8.9) | – | "Named slash command definitions" |
| `instructions` | string[] | – | "Additional paths or URLs supplying ambient instructions" |
| `references` | `Record<string, Reference>` (8.10) | – | "Named local directories or Git repositories available as external context" |
| `websearch` | `false \| { provider: "random" \| string }` | – | "Web search provider selection" |
| `plugins` | `(string \| { package: string, options?: Record<string, unknown> })[]` | – | "Ordered plugin enablement directives and external package declarations" |
| `worktree` | `{ directory: string }` (trimmed, non-empty) | data dir `worktree/<id6>` | "Directory defaults for local worktree creation" |
| `warming` | `boolean \| Warming` (8.11) | `false` | "Keep recently active sessions warm with transient model requests (default: false)" |
| `providers` | `Record<string, Provider>` (8.12) | – | (no annotation) |
| `experimental` | Experimental (8.13) | – | (no annotation) |

#### 8.1 ModelSelection (`packages/schema/src/config/model.ts:8-37`)

Input: string matching `/^[^/#]+\/[^#]+(?:#[^#]+)?$/u` (`provider/model[#variant]`) **or** `{ providerID: /^[^/#]+$/, model: /^[^#]+$/, variant?: /^[^#]+$/ }`. Decoded/encoded form is always the object. String parse (`packages/schema/src/model.ts:24-41`): provider = text before first `/`; model = text up to first `#` after it; variant = rest.

#### 8.2 Permission rule (`packages/schema/src/permission.ts:56-66`)

`{ action: string, resource: string, effect: "allow" | "deny" | "ask" }`. Ruleset = array; order matters (last match wins). Path-like actions `external_directory`, `read`, `edit` get `~`, `~/…`, `$HOME`, `$HOME/…`, `$HOME\…` expanded to `Global.home` (`config/plugin/agent.ts:141-162`); other actions (e.g. `shell`) are not expanded.

#### 8.3 Agent (`packages/schema/src/config/agent.ts:9-22`)

| Field | Type |
|---|---|
| `model` | ModelSelection |
| `request` | `{ headers?: Record<string,string>, body?: Record<string, Json> }` |
| `system` | string |
| `description` | string |
| `mode` | `"subagent" \| "primary" \| "all"` |
| `hidden` | boolean |
| `color` | string matching `/^#[0-9a-fA-F]{6}$/u` (named tokens rejected) |
| `steps` | PositiveInt |
| `disabled` | boolean (true removes the agent) |
| `permissions` | Rule[] |

#### 8.4 Formatter (`packages/schema/src/config/formatter.ts`)

`FormatterEntry = { disabled?: boolean, command?: string[], environment?: Record<string,string>, extensions?: string[] }`. `true` enables built-ins; any object also enables built-ins then overrides by name; `disabled` removes; a built-in name without `command` keeps the built-in enable check (`config/plugin/formatter.ts:29-58`).

#### 8.5 LSP (`packages/schema/src/config/lsp.ts`)

`LspEntry = { disabled: true } | { command: string[], extensions?: string[], disabled?: boolean, env?: Record<string,string>, initialization?: Record<string, unknown> }`.

#### 8.6 Media image (`packages/schema/src/config/media.ts`)

`{ auto_resize?: boolean, max_width?: PositiveInt, max_height?: PositiveInt, max_base64_bytes?: PositiveInt }`. Defaults `true`, `2000`, `2000`, `5242880` (`packages/core/src/image.ts:62-68`).

#### 8.7 MCP (`packages/schema/src/mcp.ts:7-66`)

- `McpTimeout = { startup?: PositiveInt, catalog?: PositiveInt, execution?: PositiveInt }` (ms). Defaults 30000 / 30000 / 43200000 (`packages/core/src/mcp/client.ts:34-36`).
- `Protocol = "legacy" | "auto" | "2026-07-28"` (default `"legacy"`).
- Local: `{ type: "local", command: string[], cwd?: string, environment?: Record<string,string>, disabled?: boolean, codemode?: boolean (default true), timeout?: McpTimeout, protocol?: Protocol }`.
- Remote: `{ type: "remote", url: string, headers?: Record<string,string>, oauth?: McpOAuth | false, disabled?, codemode?, timeout?, protocol? }`.
- `McpOAuth = { client_id?, client_secret?, scope?, callback_port?: int 1..65535, redirect_uri?, auth_server_metadata_url? }`.
- Server union discriminated by `type`.

#### 8.8 Compaction (`packages/schema/src/config/compaction.ts`)

`{ auto?: boolean, keep?: { tokens?: NonNegativeInt }, buffer?: NonNegativeInt }`. Defaults `auto: true`, `keep: 15000`, buffer unset ("keeps 10% free") (`packages/core/src/session/compaction.ts:42-46, 197`).

#### 8.9 Command (`packages/schema/src/config/command.ts`)

`{ template: string (required), description?, agent?, model?: ModelSelection, subagent?: boolean, subtask?: boolean ("Deprecated alias for subagent.") }`.

#### 8.10 Reference (`packages/schema/src/config/reference.ts`)

`string | { repository: string, branch?, description?, hidden? } | { path: string, description?, hidden? }`. Name must be non-empty and not match `/[\/\s`,]/` (else silently ignored). String is local when it starts with `.`, `/` or `~`, else a git repository. Local paths: `~/` → home; relative → resolved against the declaring document's directory (or `location.directory` for virtual docs) (`config/plugin/reference.ts:54-67`).

#### 8.11 Warming (`packages/schema/src/config/warming.ts`)

`boolean | { prompt?: string, interval?: DurationString, duration?: DurationString }`. Defaults: prompt `"This is a keep-alive request. Do not perform any work or use tools. Reply with exactly: OK"`, interval 4 minutes, duration 30 minutes (`packages/core/src/plugin/warming.ts:9-13`). Non-positive/non-finite → warning `"warming interval and duration must be finite positive durations"` and warming off.

#### 8.12 Provider (`packages/schema/src/config/provider.ts`)

```
Provider {
  canonical?: string            // provider ID to inherit catalog definition from
  name?: string
  env?: string[]
  package?: string
  settings?: Settings
  headers?: Record<string,string>
  body?: Record<string, Json>
  models?: Record<string, Model>
}
Settings = { timeout?: number|false, headerTimeout?: number|false, chunkTimeout?: number|false,
             compaction?: {type:"summary"}|{type:"native"}, transport?: "http"|"websocket", [k]: Json }
Model {
  modelID?: string, family?: string, name?: string,
  compatibility?: { reasoningField?: string, requireReasoning?: bool,
                    maxTokensField?: "max_completion_tokens"|"max_tokens",
                    requireFinishReason?: bool, requireAssistantAfterTool?: bool,
                    supportsPromptCacheKey?: bool, supportsThinkingBlockBinding?: bool,
                    supportsEffortUpdates?: bool },
  package?: string,
  settings?: { compaction?: …, [k]: Json }, headers?: Record<string,string>, body?: Record<string,Json>,
  capabilities?: { tools?: bool, input?: string[], output?: string[] },
  variants?: { id: string, settings?, headers?, body? }[],
  cost?: Cost | Cost[]   // Cost = { tier?: {type:"context", size:int}, input: number, output: number,
                         //          cache?: { read?: number, write?: number } }  (USD per 1M tokens)
  disabled?: boolean,
  limit?: { context?: int, input?: int, output?: int }
}
```

Application (`config/plugin/provider.ts`): configured providers get `activation = "enabled"`, an integration with method `{type:"key", label:"Manually enter API Key"}` if none exists, and `{type:"env", names: env}` when `env` given. `canonical` copies the source provider definition. Model fields overlay field-wise; `compatibility` shallow-merged; `capabilities` merged with fallback to base then defaults (`packages/core/src/model.ts:32-39`); `cost` array-normalized with cache defaults 0; `disabled → enabled = !disabled`; `limit` shallow-merged; variants merged by `id`.

#### 8.13 Experimental (`packages/schema/src/config/experimental.ts`, `policy.ts`)

`{ portable_shell_scanner?: boolean ("Defaults to false."), subagent_depth?: NonNegativeInt ("Defaults to 1."), policies?: Policy[] }`, `Policy = { action: "provider.use" | "tool.use" | "integration.use", resource: string, effect: "allow" | "deny" }`.

`skills` item resolution (`config/plugin/skill.ts:89-101`): `http(s)://` URL → remote source; `~/` → home; absolute → as is; **relative → joined to `location.directory`** (not the config file's directory).

`plugins` item resolution (`config/plugin/source.ts:113-157`): string starting with `-` → remove operation (`"-"` alone throws `"Plugin remove operation requires a target"`); `file://` → path; `./` or `../` → resolved against the declaring document's directory (or `location.directory`); bare names → packages. An absolute path that is a file (not a directory) is skipped with warning `"configured plugin path must be a directory"`.

#### 8.14 JSON `$schema` URLs

- Location config: no schema is generated in this repo; docs reference `https://opencode.ai/config.json` (`services/www/src/docs/content/config.mdx:8-9, 71-81`); the value is free-form and ignored.
- CLI config: `SchemaURL = "https://opencode.ai/v2/cli.json"` (`packages/cli/src/config/schema.ts:4`). Generated by `packages/cli/script/schema.ts` as a JSON Schema draft 2020-12 document (`$schema: "https://json-schema.org/draft/2020-12/schema"`, `$id: SchemaURL`, definitions under `$defs`, Prettier `printWidth: 120`) into `services/www/public/cli.json`. `--check` mode errors with `"Generated CLI config schema is stale. Run \`bun run generate\` from services/www."`; missing arg throws `"A schema output path is required"`.

### 9. Error reporting

#### 9.1 Normalization/parse warnings

Logged with `Effect.logWarning("configuration normalization diagnostic", { source, path, kind, action })` (`config.ts:108-113, 117-124, 128-133`):
- `source`: absolute file path, `"OPENCODE_CONFIG_CONTENT"`, or the wellknown origin.
- `path`: `"$"` for root/document-level, otherwise `` `$.${segments.join(".")}` `` (e.g. `$.provider.custom.whitelist`).
- `kind`: `invalid | conflict | unsupported`.
- `action`: the message strings from 7.1 or `"rejected malformed JSON or JSONC document"` / `"rejected canonical configuration after final validation"`.
No values (secrets) are logged (test `config.test.ts:690-781`). These are log-only; the user-facing load continues.

#### 9.2 Other log messages (verbatim)

`"failed to load wellknown config"`, `"failed to discover wellknown config"`, `"failed to reload config"` (error), `"failed to reload wellknown config"`, `"failed to reload wellknown sources"`, `"failed to refresh wellknown manifests"`, `"failed to refresh wellknown config"`, `"failed to reload MCP config"`, `"configured plugin watch failed"`, `"configured plugin path must be a directory"` (`config.ts:155, 179, 286, 298, 304, 317, 321`; `config/plugin/mcp.ts:30`; `config/plugin/source.ts:67, 152`).

#### 9.3 Fatal config errors

A `{file:}` reference failure (or a non-NotFound/PermissionDenied read error) aborts the location's config load as a defect. The failure is not cached; reloading after fixing works (test `packages/core/test/location-layer.test.ts:95-135`). Error object: `{ name: "ConfigInvalidError", data: { path, message?, issues? } }`. Renderings:
- TUI (`packages/tui/src/util/error.ts:50-66`): `` `Configuration is invalid${path && path !== "config" ? ` at ${path}` : ""}` + (message ? `: ${message}` : "") `` followed by one line per issue `"↳ " + issue.message + " " + issue.path.join(".")`.
- App (`packages/app/src/runtime/server/errors.ts:99-120`): `` `Config file at ${file} is invalid` `` or `` `Config file at ${file} is invalid: ${msg}` ``.

During a background reload, failures are caught and logged (`"failed to reload config"`); the previous config stays active (`config.ts:285-287`).

#### 9.4 `opencode debug config`

Command description `"List configuration sources"` (`packages/cli/src/commands/commands.ts:119`). Calls `GET /api/config` with `location.directory = process.cwd()` and prints `JSON.stringify(redactConfig(entries), null, 2) + EOL` (`packages/cli/src/commands/handlers/debug/config.ts:12-17`). Redaction (`debug/redact.ts:2-20`): string values whose key matches `/(?:api.?key|secret|password|token$|authorization$|cookie$|credential|private.?key)/i`, every string under a `headers` object, and `http(s)` URLs that are unparsable or contain userinfo or a secret-named query param → `"***"`. Order is not changed.

### 10. Watching and reload (`config.ts:240-327`, `packages/core/src/config/watch.ts`)

- Watch plan (`ConfigWatch.plan`, `watch.ts:8-39`):
  - recursive `directory` watches on `global` (if enabled) and each **present** `.opencode` dir, ignoring `["node_modules", ".git", "**/{node_modules,.git}/**"]`;
  - for all other candidate paths (`direct` files, all `.opencode` candidates incl. absent ones, `claude` and `agents` roots, `explicit`) not inside a watched directory: an `entries` watch on their parent with the sorted unique basenames (e.g. `[".agents", ".claude", ".opencode", "opencode.json", "opencode.jsonc"]`). Parent watches make deletion/re-creation observable.
- Watches are reconciled after every reload (keys = JSON of target; removed when no longer planned) (`config.ts:248-263`).
- Every watcher update is (a) republished on `changes()` for domain plugins and (b) triggers a reload request. Reload requests go through a sliding PubSub of size 1, debounced **100 ms** (`config.ts:244-289`). Watch readiness also triggers a rescan.
- `reload()` (serialized by a semaphore): re-run discovery + load + reconcile; if entries deep-equal the previous ones and compatibility roots unchanged → no-op; else replace and publish bus event `config.updated` (`packages/schema/src/config.ts:133-138`; `config.ts:265-279`).
- Also reloads on `Credential.Event.Switched` for a wellknown integration, on `WellKnown.Event.Updated`, and every **10 minutes** if any wellknown source exists (refresh manifests; reload if refresh reported no change, since a change already emits `WellKnown.Event.Updated`) (`config.ts:290-326`).
- Domain plugins subscribe to `config.updated` and/or filtered `changes()` and debounce 100 ms themselves (agents, commands, skills, plugins).
- `{file:}` targets are not watched separately except as part of the above; test "reloads file substitutions when their source changes" (`config.test.ts:269-301`) relies on the referenced file being under a watched root.

#### 10.1 Config update API

`PATCH /api/experimental/config` with body `Config.Patch = { shell: string | null }` (`packages/schema/src/config.ts:112-115`; `packages/protocol/src/groups/config.ts:34-45`). Implementation (`config.ts:329-350`): target dir = `initial.global ?? Global.config`; file = last **existing** of `opencode.json`, `opencode.jsonc`, else `opencode.jsonc`; read text or `"{}\n"`; `jsonc-parser` `modify(text, ["shell"], patch.shell ?? undefined, { formattingOptions: { tabSize: 2, insertSpaces: true } })` (null removes the key, comments preserved); ensure trailing `\n`; write (creating dirs); request reload. Unavailable → defect `"Config updates are unavailable"` (`packages/server/src/handlers/config.ts:14`).

---

### 11. CLI / TUI configuration (`cli.json`)

#### 11.1 Location and load (`packages/cli/src/config/config.ts`)

- File: `path.join(Global.config, "cli.json")` (`config.ts:31`) – global only, no project lookup.
- `get()`: under a cross-process file lock (`Flock.acquire(file, { dir: <state>/locks })`, `config.ts:52-63`), run migration (11.4), then `merge(fileInfo, inlineContent)` (`config.ts:64-81`). Migration failures log `"failed to migrate cli config"`; persistence failure logs `"failed to persist migrated cli config"` and uses the migrated value in memory.
- Parsing: JSONC with `allowTrailingComma: true`; any parse error → treated as absent; root must be a record; then **whole-file** decode against `Info` – any invalid field makes the whole file be ignored (treated as `{}`) (`config.ts:22-24, 36-40, 144-149`).
- Inline content: `OPENCODE_CLI_CONFIG_CONTENT`, parsed the same way, read once at layer construction.
- `merge(...values)` (`config.ts:118-138`): each value first passes `normalizeLegacyTabs` (11.3); then a **recursive deep merge of plain objects; arrays and scalars replace**; result decoded (fallback `{}`). Test (`packages/cli/test/config.test.ts:74-117`): `tabs` merged, `keybinds` merged per key, `plugins` array replaced.
- `update(fn)`: migrate (failure is fatal here), compute `next = immer.produce(current, fn)`, diff `current` vs `next` into leaf edits (`changes`, `config.ts:151-168`; objects recurse, arrays/scalars replaced whole, removed keys → `undefined`), apply each with `jsonc-parser` `modify` (2-space) to the **file text** (or, if missing, to `JSON.stringify({ $schema: SchemaURL }, null, 2)`), re-parse + decode; on failure `"Invalid CLI config update"`; errors wrapped as `"Failed to update CLI config"` (`config.ts:83-110`). Write = `cli.json.tmp` (mode 0600) then rename; trailing newline ensured. Returned value = `merge(written, inlineContent)` (inline content still wins, test `config.test.ts:74`). Comments are preserved (test `config.test.ts:542`).
- TUI reload: `fs.watch(dirname(cli.json))`; any event → `get()` → re-resolve; errors swallowed (`packages/tui/src/config/index.tsx:356-365`).

#### 11.2 Schema (`packages/cli/src/config/schema.ts`, `packages/tui/src/config/index.tsx:69-249`)

`Info = { $schema?: string ("JSON Schema for CLI configuration"), ...Config.Info.fields }`. All optional:

| Key | Type | Resolved default (`index.tsx:281-333`) |
|---|---|---|
| `theme` | `{ name?: string, mode?: "system"\|"dark"\|"light" }` | – |
| `keybinds` | `{ [command]: BindingValue }` (11.5) | definitions' defaults |
| `plugins` | `(string \| { package: string, options?: Record<string, any> })[]` | – |
| `leader` | `{ timeout?: int > 0 }` | `2000` |
| `scroll` | `{ speed?: number ≥ 0.001, acceleration?: boolean }` | – |
| `attention` | `{ notifications?: bool, sound?: bool, volume?: 0..1, sound_pack?: string, sounds?: Partial<Record<"default"\|"question"\|"permission"\|"error"\|"done"\|"subagent_done", string>> }` | `false`, `false`, `0.4`, `"opencode.default"`, `{}` |
| `diffs` | `{ source?: "working"\|"branch"\|"committed"\|"turn", wrap?: "word"\|"none", tree?: bool, single?: bool, view?: "auto"\|"split"\|"unified" }` | source "branch" (per description) |
| `terminal` | `{ title?: bool, copy?: "manual"\|"select" }` | – |
| `prompt` | `{ editor?: bool, paste?: "compact"\|"full", image_preview?: bool }` | – |
| `session` | `{ sidebar?: "auto"\|"hide", scrollbar?: bool, thinking?: "show"\|"hide", grouping?: "auto"\|"none", verbosity?: "low"\|"medium"\|"high", image_preview?: bool, tps?: bool, markdown?: "source"\|"rendered", new_location?: "launch"\|"inherit", permissions?: "prompt"\|"autoaccept" }` | `new_location "launch"`, `permissions "prompt"`, `tps true` |
| `tabs` | `{ mode?: "auto"\|"on"\|"off", enabled?: bool (legacy), scope?: "global"\|"cwd", layout?: "horizontal"\|"vertical", indicators?: "status"\|"numbers" }` | mode `auto` (or from `enabled`), scope `cwd`, layout `horizontal`, indicators `status`; `enabled = mode==="on" \|\| (mode==="auto" && env.HERDR_ENV !== "1")` |
| `mini` | `{ thinking?, tools?, shell_output?, turn_summary?, footer?, splash?: "show"\|"hide", work_spinner?: <14 literals, default "block-soft-slide">, mono?: bool, replay?: bool, replay_limit?: int > 0 }` | – |
| `debug` | `{ devtools?: bool, timing?: bool, turn_tokens?: bool \| "verbose" }` | – |
| `experimental` | `Record<string, boolean>` | – |
| `animations` | boolean | – |
| `mouse` | boolean | `true` |
| `cursor` | `{ style?: "block"\|"underline"\|"line"\|"default", blinking?: bool }` | if present: style `block`, blinking `true` |

`work_spinner` literals: `block-soft-slide, block-soft-sweep, block-low-comet, block-low-duet, block-shuttle, block-bridge, block-squeeze, small-toggle, square-toggle, grow-shrink, quadrant-orbit, crosshatch, density-wave, seed` (`index.tsx:29-44`).

When the terminal cannot suspend, `terminal.suspend` is forced to `"none"` and, unless the user set `input.undo`, it becomes `"ctrl+z," + default` de-duplicated (`index.tsx:288-296`).

#### 11.3 Legacy tabs

`normalizeLegacyTabs` (`packages/cli/src/config/schema.ts:12-18`): if `tabs.enabled` defined → `tabs.mode ??= enabled ? "on" : "off"`, delete `enabled`. Only in memory; the file is not rewritten (test `config.test.ts:119-137`).

#### 11.4 Migration (`packages/cli/src/config/migrate.ts`)

**A. `cli.json` exists** (`migrate.ts:37-98`): parse JSONC (errors → no-op); in-place text edits:
1. `terminal.copy_on_select: bool` → if `terminal.copy` undefined, set `terminal.copy = bool ? "select" : "manual"`; always remove `copy_on_select`.
2. If more than one top-level `keybinds` property, remove all but the last.
3. For each key in `keybinds`: `target = v1 CommandMap[name] ?? (name ∈ v2 Definitions || name ∈ values(CommandMap) ? name : undefined)`; unknown → untouched. Duplicate properties: earlier ones removed, last kept. If `target ∉ Definitions` (e.g. `app.heap_snapshot`) or (renamed and target already present) → all removed; else renamed to target (key text replaced). Comma handling preserves comments (`removeProperty`, `migrate.ts:130-158`).
4. Persist only if text changed **and** the result decodes; write to `${file}.${pid}.${uuid}.tmp` (0600) then rename; temp removed in all cases.

**B. `cli.json` absent** (`migrate.ts:100-116`): read `<config>/tui.json` (JSON with trailing commas; whole-file decode against `TuiConfigV1.Info`, failure → ignored) and `<state>/kv.json`. `migrateV1(legacy, kv)` (`migrate.ts:160-258`); if it yields no keys, nothing is written; else write `{ $schema: "https://opencode.ai/v2/cli.json", ...values }` (2-space JSON + `\n`) and log `"migrated cli config"` with `from` and `to`. Source files are kept.

`migrateV1` mapping:

| Source | Target |
|---|---|
| `tui.plugin[]` (string or `[pkg, opts]`) | `plugins` (tuple → `{package, options}`) |
| `tui.plugin_enabled: {id: bool}` | appended to `plugins`: `id` or `"-" + id` |
| `tui.theme ?? kv.theme` | `theme.name` |
| `kv.theme_mode_lock` | `theme.mode` |
| `tui.keybinds` | `keybinds`, renamed via CommandMap; names not in v2 Definitions dropped |
| `tui.leader_timeout` | `leader.timeout` |
| `tui.scroll_speed` | `scroll.speed` |
| `tui.scroll_acceleration.enabled` | `scroll.acceleration` |
| `tui.attention` (all fields, incl. legacy `enabled`) + `kv.attention_sound_pack` | `attention` (+ `sound_pack`) |
| `kv.diff_wrap_mode` | `diffs.wrap` |
| `kv.diff_viewer_show_file_tree` | `diffs.tree` |
| `kv.diff_viewer_single_patch` | `diffs.single` |
| `kv.diff_viewer_view ?? (tui.diff_style === "stacked" ? "unified" : undefined)` | `diffs.view` (`diffs` object created whenever `tui.diff_style` is defined) |
| `kv.terminal_title_enabled` | `terminal.title` |
| `kv.file_context_enabled` | `prompt.editor` |
| `kv.paste_summary_enabled` | `prompt.paste = true ? "compact" : "full"` |
| `kv.sidebar` | `session.sidebar` |
| `kv.scrollbar_visible` | `session.scrollbar` |
| `kv.thinking_mode ?? (kv.thinking_visibility ? "show" : "hide")` | `session.thinking` |
| `kv.exploration_grouping` | `session.grouping = true ? "auto" : "none"` |
| `kv.animations_enabled` | `animations` |
| `tui.mouse` | `mouse` |
| `tui.cursor` | `cursor` |
| `tui.prompt`, other kv keys (`skipped_version`, `which_key_layout`, `dismissed_getting_started`, …) | dropped |

#### 11.5 Keybinds (`packages/tui/src/config/keybind.ts`)

`BindingValue = false | "none" | BindingItem | BindingItem[]`, `BindingItem = string | KeyStroke | { key: string | KeyStroke, event?: "press"|"release", preventDefault?: bool, fallthrough?: bool, [k]: unknown }`, `KeyStroke = { name, ctrl?, shift?, meta?, super?, hyper? }` (`keybind.ts:8-33`). Strings are comma-separated alternatives; `<leader>` expands to the `leader` binding (default `"ctrl+x"`). Override keys are the dotted command names of `Definitions` (`keybind.ts:45-299`, ~230 entries; default and description per entry – port the table verbatim). Unknown keys are stripped by schema decode; `TuiKeybind.parse` would throw `` `Unrecognized keybind${n === 1 ? "" : "s"}: ${names.join(", ")}` `` (`keybind.ts:337-346`).

The v1 → v2 key map is `TuiKeybind.CommandMap` in `packages/tui/src/config/v1/keybind.ts:251-412` (snake_case → dotted, e.g. `app_exit→app.exit`, `command_list→command.palette.show`, `editor_open→prompt.editor`, `theme_list→theme.switch`, `sidebar_toggle→session.sidebar.toggle`, `status_view→opencode.status`, `messages_page_up→session.page.up`, `messages_undo→session.undo`, `display_thinking→session.toggle.thinking`, `input_clear→prompt.clear`, `input_paste→prompt.paste`, `history_previous→prompt.history.previous`, `model_provider_list→model.dialog.provider`, `model_favorite_toggle→model.dialog.favorite`, `app_heap_snapshot→app.heap_snapshot` (dropped: not a v2 definition); `leader` stays `leader`).

---

### Open questions

1. **Docs vs schema for policy actions**: `config.mdx` shows `{ "action": "permission", "resource": "shell:git push *" }` but `ConfigPolicy.Info.action` only accepts `"provider.use" | "tool.use" | "integration.use"` (`packages/schema/src/config/policy.ts:9`); a `permission` statement is dropped as invalid. Replicate the code (drop), presumably.
2. **No location-config JSON schema is generated in-repo**; `https://opencode.ai/config.json` is served externally (likely the v1 schema). Should the Rust port generate one from the v2 `Info`?
3. **Explicit `OPENCODE_CONFIG` ranks below project files** (`config.ts:220-237`) (comment says so explicitly). Confirm this is the desired user-facing behavior.
4. **`{env:}` values are not JSON-escaped** while `{file:}` values are; also `{file:}` comment-skip only handles whole-line `//`. Replicate exactly, or treat as bugs?
5. **`{file:~/…}` uses `os.homedir()`** while other `~` expansions use `Global.home` (`OPENCODE_TEST_HOME`). Mismatch is observable only in tests.
6. **Wellknown `{file:}` base dir is the origin URL string** (`dir: entry.origin`), so relative `{file:}` in remote config resolves to a nonsense local path. Probably unintended.
7. **The upward walk has no stop** (goes to `/`), so `~/opencode.json`, `~/.opencode/` and any ancestor configs above the git root are loaded for every project under `$HOME`. Matches docs ("to the filesystem root").
8. **Keys accepted with no consumer** in core (`instructions`, `lsp`, `share`, `enterprise`, `username`, `update` in project files). Should the port keep them as validate-only?
9. **CLI `migrateV1` persists the raw migrated object without decoding** (e.g. legacy `attention.enabled` is written to `cli.json` and later ignored on decode). Replicate?
10. `skills` relative paths resolve against `location.directory`, whereas `plugins` and `references` resolve against the declaring file's directory. Inconsistent but in code.
11. Exact `Watcher` "entries" semantics (whether events fire for non-listed names, rename handling) live in `packages/core/src/filesystem/watcher.ts` and are not specified here.
12. The full ~230-entry keybind default table (`packages/tui/src/config/keybind.ts:45-299`) and the v1 definitions table should be ported verbatim from source; this section summarizes them only.

---

## 7. Background service model

The CLI is a thin client of a local HTTP server. By default every client command talks to one
long-lived **background service** per user per channel, started on demand
(`opencode serve --service`). Alternatives: a private **standalone** child server
(`--standalone`, or the service is disabled), or an explicit remote server (`--server <url>`).

### 7.1 Files

Let `name = filename(channel)` (`cli/src/services/service-config.ts:38-41`):

| Channel | `name` |
|---|---|
| `latest`, `dev`, `beta`, `next` | `service.json` |
| any other (incl. `local`) | `service-<sanitize(channel)>.json` (e.g. `service-local.json`, `preview/a` → `service-preview-a.json`) |

| File | Path | Owner | Mode |
|---|---|---|---|
| Service **config** | `<config>/<name>` | CLI (`service set/unset`, password persistence) | 0600, written via `<file>.tmp` + rename, `JSON.stringify(value, null, 2) + "\n"` |
| Service **registration** (discovery contract) | `<state>/<name>` | the running service | 0600, written via `<file>.<instanceId>.tmp` + rename |
| PTY handoff sidecar | `<state>/<name>.pty-handoff` | client stopping a service | 0600, via `<file>.pty-handoff.<uuid>.tmp` (`wx`) + rename |

Client-library default when no file is given: `$XDG_STATE_HOME ?? ~/.local/state` + `/opencode/service.json`
(note `??`, not `||`; `client/src/service-probe.ts:39-41`).

**Service config schema** (`service-config.ts:15-25`) — JSON object, all optional:

```jsonc
{
  "disabled": boolean,
  "remote": { "route": string },      // present iff remote access is on; route = 16 hex chars (8 random bytes)
  "hostname": string,
  "port": integer 1..65535,
  "password": string,
  "cors": [string],
  "env": { [name: string]: string }
}
```
Reading: missing or undecodable → `{}`. Legacy form with `"remote": true|false` is repaired in
place: `true` → `remote: {route: <new random>}`, `false` → key removed; rewritten to disk.

**Registration schema** (`client/src/effect/service.ts:142-148`):

```jsonc
{ "id": string?, "version": string?, "url": string, "pid": int > 0, "password": string? }
```
Written by the service as `{id: randomUUID(), version: OPENCODE_VERSION, url: "http://<host>:<port>", pid, password}`
(`cli/src/services/service-registration.ts:526-533`). Missing/corrupt file = "no service".
Two registrations are the **same** instance iff `id`, `version`, `url`, `pid` are all equal.

**Legacy migrations** run on every `ServiceConfig.options()` / `read()`:
- Config: if channel is not `latest`/`local`, legacy config `<config>/service-<sha1hex(channel)>.json`
  is copied to the new config path if it decodes and the new file does not exist (`wx`).
- Registration: candidates `<state>/service-<sha1hex(channel)>.json` (non-latest/local) and, for
  non-release, non-local channels, `<state>/service.json`; copied (`wx`, 0600) to the new
  registration path only if the registered `version` belongs to this channel: equal to the running
  version, or `0.0.0-<channel>-<digits>[.<digits>]` (`service-config.ts:54-64`).

### 7.2 Default port (`service-config.ts:43-47`)

| Channel | Port |
|---|---|
| `latest`, `dev`, `beta`, `next` | `0xC0DE` = 49374 |
| `local` | `0xC0DF` = 49375 |
| other | `10000 + (parseInt(sha1hex(channel)[0..8], 16) % 50000)` |

On Windows (`service-config.ts` `defaultPort`, 2.1) a port of `0x8000` or more becomes
`0x7000 | (port & 0xfff)`, below the ranges Windows, Hyper-V, WinNAT and WSL mirrored networking
reserve: `latest` 28894, `local` 28895. A port saved in the service config is kept.

### 7.3 Service-mode server startup (`serve --service`) — the election

`cli/src/server-process.ts:47-204`. Single-instance election is done by **binding the fixed port**,
not by a lock file.

1. Consume and delete `OPENCODE_PTY_HANDOFF` from env; decode as `PersistentPty.Handoff`
   (`{directory, instanceID, ticket, expiresAt}`); invalid → warning
   `Ignoring invalid PTY restart handoff; persistent terminals will start fresh`.
2. `chdir(<home>)`.
3. `options = ServiceConfig.options()` (registration path, no version requirement, env from config,
   command), `config = ServiceConfig.read()`.
4. `hostname = --hostname ?? config.hostname ?? "127.0.0.1"`;
   `port = --port ?? config.port ?? defaultPort(channel)`.
5. **Incumbent check**: `Service.incumbent({file, url: serviceURL(hostname, port)})` where
   `serviceURL` = `http://<host>:<port>` with IPv6 hosts bracketed. Reads the registration and probes
   `serviceURL/api/info` (not the registered URL) with the registered password. If an authenticated,
   compatible owner answers (any state: ready/waiting/failed) → return immediately; the process exits 0.
6. Windows + package-manager/curl install: keep a hard link of the running exe in `<cache>`
   (`RetainedImage`, file `opencode-service-<pid>-<8hex>.exe`) so installers can replace the binary.
7. `password = config.password || randomBytes(32).base64url`.
8. Start HTTP server (`@opencode/server/process`). On `EADDRINUSE`: for up to 15 s loop
   {re-check incumbent (bounded by remaining time) → if found return (exit 0); sleep 100 ms; retry bind}.
   Timeout → fail with
   ``Managed service port <port> on <hostname> is already in use by another process. Configure another port with `opencode service set port <port>` and start the service again.``
9. **On listen** (before boot completes): if config had no password, persist it
   (`ServiceConfig.password(password)` → writes `password` into service config). Then register:
   write registration atomically; start a watchdog that every **5 s** re-reads the registration;
   if it cannot be read/decoded (warning `managed service registration check failed; shutting down`)
   or is no longer this instance (all of id, version, url, pid, password equal; warning
   `managed service registration replaced; shutting down`) → trigger graceful shutdown.
   On shutdown, remove the registration file only if it still describes this instance.
10. If `config.remote` set: start the OpenTunnel remote tunnel targeting the bound address
    (wildcard binds `0.0.0.0`/`::` → `127.0.0.1:<port>`); remote URL appended to `/api/info` `urls`.
11. stdout `server listening on <url>` (stdout of contenders is discarded). Wait until shutdown
    is requested (registration loss) or the process is signalled.

Server-side `/api/info` (`server/src/process.ts:188-217`): requires Basic auth even before ready;
body `{"version","pid","urls":[...],"paths":{"tmp"}}`; status 200 ready, 500 failed, 503 starting/stopping
(with `retry-after: 1`). Before ready, other routes return 503
`{"code":"service_starting"|"service_stopping"}` (`retry-after: 1`) or, if boot failed,
503 `{"code":"service_failed","message":"The background service could not start.","action":"Run \`opencode service restart\` after checking the service logs."}`.
Unauthorized: 401 `{"_tag":"UnauthorizedError","message":"Authentication required"}` with
`www-authenticate: Basic realm="Secure Area"` unless `sec-fetch-mode` is present and not `navigate`.
Credentials may also come from query `auth_token=<base64(user:pass)>` or a same-origin session cookie.
Username is always `opencode`. A service whose boot fails stays alive in `failed` state (never exits by itself).

### 7.4 Client-side `Service.ensure` (`client/src/effect/service.ts:49-127`)

Timing constants (`client/src/service-timing.ts`): poll 25 ms; probe timeout 2 000 ms; base spawn
delay 5 000 ms (doubles, max 30 000 ms); overall wall-clock limit 120 000 ms; stop poll 50 ms × 100.

**Probe** (`probeResult`, `service-probe.ts:47-88`): `GET <registered url>/api/info` with Basic auth
(if registration has a password), 2 s timeout.
- network error/timeout → no service (`timedOut` = aborted).
- 404 → service `{state: ready, compatible: false, version: registration.version}`.
- otherwise body must be JSON with string `version` and integer `pid ≥ 0`, else no service.
  `body.pid ≠ registration.pid` → no service; `registration.version` defined and ≠ `body.version` → no service.
  State: 2xx → `ready`, 500 → `failed`, other → `waiting`. `compatible = true`.

**Loop** (every 25 ms until success or 120 s):
1. Read registration + probe.
2. If the probe timed out for the same registration on **3 consecutive** iterations: announce
   `missing`, log `Background service is unresponsive; recovery cannot preserve persistent terminals`,
   delete the PTY sidecar, terminate that pid (§7.5), let a new contender spawn immediately.
3. If a service answered:
   - Version requirement set and `service.version !== required` → **replace**: announce
     `version-mismatch` with the old version; warn `Background service is not ready; replacement cannot preserve persistent terminals`
     if not ready; `stop({pty: ready ? "handoff" : "clear"})`; continue.
   - not compatible → fail `Background service uses an incompatible health protocol. Update this client or explicitly restart the service.`
   - ready → clear stale PTY sidecar (if it belongs to another instance) and **return endpoint** `{url, auth: basic opencode/<password>}`.
   - failed → fail `Background service failed to start`.
   - waiting → continue.
4. No service: reap finished contenders; if all contenders are gone and one failed → fail with its error:
   spawn error `Failed to start server`; non-zero exit `Server process exited with code <n>` +
   (`\n` + last ≤8 KiB of its stderr, trimmed, if any); signal `Server process terminated by <SIGNAL>`.
   A contender exiting 0 (lost election) doubles the spawn delay.
5. Recruit when: no failure recorded, < 2 live contenders, and ≥ spawnDelay since last spawn
   (if a registration exists but has not answered yet, the clock starts at first observation, so
   an existing registration gets 5 s before a competitor is spawned; with no registration the first
   spawn is immediate). Announce `missing` (once per ensure call), spawn contender.
6. Timeout → previous failure or `Timed out waiting for the background service to start`.

**Contender process**: `command` = self command + `["serve","--service"]` where self command is
the executable itself for compiled binaries (`[execPath]`), or `[bun|node, (node flags), entry script]`
in dev (`cli/src/util/process.ts`). Spawned detached, hidden window, stdio
`[ignore, ignore, pipe(stderr)]`, env = current env + service-config `env` +
`OPENCODE_PTY_HANDOFF=<JSON handoff>` when a valid unexpired sidecar for the current registration
exists. The parent unrefs it (does not wait).

`onStart(reason, previousVersion)` is invoked at most once per ensure call.

### 7.5 `Service.stop` / terminate (`client/src/effect/service.ts:130-197`)

1. Read registration. If `pty === "handoff"` and registered: prepare PTY handoff
   (`POST /api/experimental/persistent-pty/handoff`, response `{handoff: Handoff|null}`; published to the
   sidecar `{source:{id,pid,url}, handoff, expiresAt: handoff.expiresAt ?? now+30s}`; on failure
   log `Background service cannot hand off persistent terminals; shutting them down before replacement`
   and POST `/api/experimental/persistent-pty/shutdown`). Otherwise delete the sidecar. Errors only warn
   (`Failed to prepare persistent terminals for replacement`).
2. Terminate: re-read registration; if absent or not the same instance → done. `SIGTERM` pid; poll
   `kill(pid, 0)` every 50 ms up to 100 times; still alive → `SIGKILL` and poll again (failure:
   `Server process <pid> is still running`). Then delete the registration file if it still describes
   that instance.

### 7.6 `ServerConnection.resolve` (`cli/src/services/server-connection.ts`)

```
resolve({server, standalone, mismatch = "ignore", onStart}):
  if server && standalone: fail "--server and --standalone cannot be combined"
  if server:
      password = $OPENCODE_PASSWORD ?? $OPENCODE_SERVER_PASSWORD   (optional)
      endpoint = {url: server, auth: password ? basic("opencode", password) : none}
      GET <server>/api/info  (5 s timeout)
        401           -> "Server at <url> requires a password; set OPENCODE_PASSWORD"   (no password given)
                         "Server at <url> rejected the password"                        (password given)
        transport err -> "Could not reach server at <url>"
        other err     -> "Server at <url> did not provide a compatible V2 health response"
      if info.version != OPENCODE_VERSION:
        stderr "Warning: Server at <url> has version <v>; this client is <ours>. Continuing anyway.\n"
      return {endpoint}                       // no managed-service controls
  if standalone || serviceConfig.disabled == true:
      return {endpoint: Standalone.start()}
  options = ServiceConfig.options({checkVersion: mismatch != "ignore"})   // version = OPENCODE_VERSION or none
  endpoint = mismatch == "replace" ? ensure(options)
           : mismatch == "ignore"  ? ensure(options without version)
           : /* "error" */ discover(options) ?? (discover(any) ? fail "Background server version does not match this client" : ensure(options))
  return {endpoint, service: {reconnect: ensure(no version), restart: stop(pty handoff) then ensure(no version)}}
```

### 7.7 Standalone server (`cli/src/services/standalone.ts`)

1. `password = randomBytes(32).base64url`.
2. Spawn `<self> serve --stdio --port 0` with cwd = the CLI's startup cwd, env = inherited +
   `OPENCODE_PASSWORD=<password>` (overrides any inherited value), stdin **pipe** (ownership lease),
   stdout pipe, stderr inherited if `OPENCODE_PRINT_LOGS=1` else ignored; kill with SIGTERM,
   force-kill after 3 s when the CLI scope closes.
3. The first stdout line must be JSON `{"url": string}`; stdout keeps being drained afterwards.
   If stdout ends first → `Standalone server exited before reporting readiness`.
4. Endpoint `{url, auth: basic("opencode", password)}`.

Server side of `--stdio` (`server-process.ts`): deletes `OPENCODE_PASSWORD` and
`OPENCODE_SERVER_PASSWORD` from its env (so tools don't inherit it) after reading the password from
them; hostname `--hostname` or `127.0.0.1`; prints exactly one stdout line `{"url":"http://127.0.0.1:<port>"}`;
exits when its stdin reaches EOF/closes (owner died).

### 7.8 `serve` (`handlers/serve.ts`, `server-process.ts`)

- `--service` and `--stdio` together → error `--service and --stdio cannot be combined`.
- Mode `default` (foreground): hostname `--hostname ?? "127.0.0.1"`; port `--port`, or if absent
  scan upward from **4096** until a free port (`server/src/process.ts:142-149`); `--cors` origins added.
  Password = `$OPENCODE_PASSWORD ?? $OPENCODE_SERVER_PASSWORD`, else random 32-byte base64url.
  stdout: `server listening on http://<host>:<port>` then, only if the password was generated,
  `server password <password>`. Runs until killed.
- Mode `stdio`: §7.7. Mode `service`: §7.3 (`--hostname/--port/--cors` override service config).
- Server options passed from env (all modes): `OPENCODE_SIMULATE` (truthy = `"1"` or case-insensitive
  `"true"`), models `{url: $OPENCODE_MODELS_URL, file: $OPENCODE_MODELS_PATH, fetch: !truthy($OPENCODE_DISABLE_MODELS_FETCH)}`,
  config `{directory: $OPENCODE_CONFIG_DIR, project: !truthy($OPENCODE_CONFIG_PROJECT_DISABLE ?? $OPENCODE_DISABLE_PROJECT_CONFIG), file: $OPENCODE_CONFIG, content: $OPENCODE_CONFIG_CONTENT}`,
  windows `{gitbash: $OPENCODE_GIT_BASH_PATH}`,
  fs `{filewatcher: !truthy($OPENCODE_FILEWATCHER_DISABLE ?? $OPENCODE_DISABLE_FILEWATCHER), fff: $OPENCODE_DISABLE_FFF unset ? platform != win32 : !truthy($OPENCODE_DISABLE_FFF)}`,
  database path §5, app `{name: $OPENCODE_CLIENT ?? "cli", version, channel}`.
- Server logs are annotated `role=server` and go to the same log file (§16).

### 7.9 `service` subcommands (`handlers/service/*.ts`, `service-config.ts`)

All `service` commands use `ServiceConfig.options()` (no version requirement).

| Command | Behavior | stdout |
|---|---|---|
| `service start` | `ensure` (reuses any running compatible service regardless of version) | `<url>` + EOL |
| `service status` | `discover` (read + probe, no spawn): ready & compatible → URL | `<url>` + EOL, or `stopped` + EOL |
| `service stop` | best-effort `POST /api/experimental/persistent-pty/shutdown` to the discovered service; `stop` (pty clear) | nothing |
| `service restart` | same as stop, then `ensure` | `<url>` + EOL |
| `service get` | see below | value + EOL |
| `service set` / `unset` | see below | nothing |

Valid keys: `disabled remote hostname port password cors env`; anything else →
`Unknown service config key: <key>`.

`service get [key] [name]`:
- no key: config without `password`; if `remote` present it is replaced by `{}`;
  `JSON.stringify(…, null, 2)` (e.g. `{}`).
- `name` given for non-`env` key → `Usage: opencode service get <key>`.
- `disabled` → `"true"`/`"false"` (default false); `remote` → `"true"` iff present; `hostname` → value or
  empty string; `port` → number or empty; `password` → existing password, **creating and persisting a new
  random one if absent**; `cors` → JSON array pretty-printed (`[]` default); `env` → whole map pretty JSON,
  or with `name` the value or empty string.

`service set <key> <value> [env-value]`:
- `env-value` for non-`env` key → `Usage: opencode service set <key> <value>`.
- `disabled`: value must be `true`/`false` (`Disabled must be true or false`); `true` stops the service
  first; writes `disabled: <bool>`.
- `remote`: `true`/`false` (`Remote must be true or false`). `true` first ensures the OpenTunnel tunnel
  can be created, stops the service, then creates (or keeps) `remote.route`; `false` stops and removes `remote`.
- `hostname`: stop, write.
- `port`: integer 1..65535 else `Port must be between 1 and 65535`; stop, write.
- `password`: stop, write.
- `env <NAME> <value>`: missing value → `Usage: opencode service set env <key> <value>`; stop; merge.
- `cors <a,b>`: split on `,`, trim; each must parse as URL with `http:`/`https:` and equal its own
  `origin` (no path/trailing slash) else
  `CORS must be a comma-separated list of HTTP(S) origins without paths or trailing slashes`; stop; write.

`service unset <key> [name]`: `name` for non-env → `Usage: opencode service unset <key>`.
Every key except `disabled` stops the service first, then the key is removed. `env` requires
`name` (`Usage: opencode service unset env <key>`); removes that entry and drops `env` entirely when it
becomes empty.

Service stop/start is never automatic after `set`: the next client command re-spawns the service
with the new settings.

---

## 8. Default command `$` — TUI launch (`handlers/default.ts`)

1. If `directory` given: `chdir(directory)` (errors propagate, exit 1).
2. Create an update preflight UI (`UpdatePreflight`, see §11).
3. Resolve server with mismatch `replace` and `onStart(reason, prev)`:
   - log `background service starting` `{reason, previousVersion}`;
   - for `version-mismatch`, if the preflight UI takes over (`preflight.begin(prev)` true) print nothing;
     otherwise stderr `Restarting background server (version mismatch)...\n`; for `missing`:
     `Starting background server...\n`.
   - resolution failure → preflight shows `OpenCode update could not start the new background service`, error propagates.
4. `--session <id>`: look the session up (`findSession`, `cli/src/session-target.ts`); if it exists it is
   resumed (`sessionID`), otherwise the id becomes `newSessionID` — the session created by the first
   prompt receives this id.
5. Start the background update checker: run `updater.run` immediately and then every 10 minutes,
   serialized with manual checks/applies by a 1-permit semaphore.
6. Launch the TUI (`@opencode/tui` `run`) with:
   - `app = {name: $OPENCODE_CLIENT ?? "cli", version, channel: $OPENCODE_TUI_CHANNEL ?? channel}`
   - `server = {endpoint, service?: {reconnect, restart}}` (service controls only for the managed service)
   - `args = {continue, sessionID, newSessionID, prompt, auto: auto||yolo||dangerouslySkipPermissions}`
   - `config` = CLI config (`cli.json`) path/get/update
   - `updater = {remote: --server given, subscribe, check, apply}`
   - `packages.prepare(spec, install=true)` → npm add/resolve
   - `environment` = full process env minus `OPENCODE_PASSWORD`/`OPENCODE_SERVER_PASSWORD`, only when
     `--server` is not given (sent to the server as the session environment)
   - `terminalHandoff` = `preflight.finish()`; `log(level, message, tags)` → Effect logger.
7. Exit when the TUI exits.

---

## 9. `run` command (non-interactive)

All paths are relative to `packages/cli/` unless they start with `packages/`.

### Command definition and flags

Defined at `src/commands/commands.ts:376-417`. Description: `Run OpenCode with a message`.

| Param | Kind | Alias | Default | Description (verbatim) |
|---|---|---|---|---|
| `message` | positional, variadic (min 0) | | `[]` | `Message to send` |
| `--standalone` | bool | | false | `Run with a private server instead of the background service` |
| `--server <url>` | string, optional | | | `Connect to a server URL instead of the background service` |
| `--continue` | bool | `-c` | false | `Continue the last session` |
| `--session <id>` | string, optional | `-s` | | `Session ID to continue, or to create if it does not exist` |
| `--fork` | bool | | false | `Fork the session before continuing` |
| `--model <ref>` | string, optional | `-m` | | `Model to use in the format provider/model#variant` |
| `--agent <name>` | string, optional | | | `Agent to use` |
| `--format default\|json` | literal | | `default` | `Output format` |
| `--file <path>` | string, repeatable (at most 100) | `-f` | `[]` | `File to attach to the message` |
| `--title <title>` | string, optional | | | `Session title` |
| `--thinking` | bool | | false | `Show thinking blocks` |
| `--auto` | bool | | false | `Auto-approve permissions that are not explicitly denied` |
| `--yolo` | bool, hidden | | false | (none) |
| `--dangerously-skip-permissions` | bool, hidden | | false | (none) |

Global flag `--print-logs` (`src/commands/commands.ts:6-11`): when set, the runtime sets `OPENCODE_PRINT_LOGS=1` before loading the handler (`src/framework/runtime.ts:98`).

Help output must not contain `--interactive`, `--variant`, `--attach`, or `--command`, and must contain `--server string` (`test/mini.test.ts:134-143`).

### Handler (`src/commands/handlers/run.ts:6-31`)

1. Lazily imports `runNonInteractive` from `src/run/run.ts`.
2. **`--` passthrough:** `separator = process.argv.indexOf("--", 2)`. The final message array is `[...input.message, ...(separator === -1 ? [] : process.argv.slice(separator + 1))]`. Option-like text after `--` therefore becomes message text (`test/mini.test.ts:145-150`). The parser does not bind tokens after `--` to `message` (see Open questions).
3. Resolves the server with `ServerConnection.resolve({ server, standalone })`. `mismatch` is not passed, so it defaults to `"ignore"` (`src/services/server-connection.ts:21-50`):
   - `--server` together with `--standalone` fails with `--server and --standalone cannot be combined`.
   - With `--server`: the endpoint uses Basic auth `opencode:<OPENCODE_PASSWORD or OPENCODE_SERVER_PASSWORD>` when either is set. The handler calls `server.info` with a 5 s timeout. If the version differs from the client's, stderr gets `Warning: Server at <url> has version <v>; this client is <OPENCODE_VERSION>. Continuing anyway.\n`. Connect errors (verbatim): `Server at <url> requires a password; set OPENCODE_PASSWORD` (401 and no auth), `Server at <url> rejected the password` (401 with auth), `Could not reach server at <url>` (transport), `Server at <url> did not provide a compatible V2 health response` (anything else).
   - `--standalone`, or a service config with `disabled === true`: starts a private standalone server.
   - Otherwise it uses the managed background service (`Service.ensure`, ignoring the version), and `Resolved.service` is set.
   - These failures happen **before** `runNonInteractive`. They are Effect failures that reach the top-level reporter, not `reportRunError`, so even `--format json` produces no JSON error line.
4. Calls `runNonInteractive` with `auto = input.auto || input.yolo || input.dangerouslySkipPermissions` (`src/commands/handlers/run.ts:27`), plus the other fields mapped directly.

### `runNonInteractive` (`src/run/run.ts`)

`runNonInteractive(input)` = `runNonInteractiveWithOptions(input, {})` (`run.ts:62-69`). Any rejection of `run()` is caught and reported through `reportRunError(input, errorMessage(error))` with no session ID (`run.ts:67-69`). `errorMessage` returns `error.message` for an `Error`, then `.message` for any object with a string `message`, then `String(error)` (`src/util/error.ts`).

#### Preparation order (`run.ts:71-81`)

1. If `fork && !continue && !session`, fail with `--fork requires --continue or --session`.
2. `root = options.root ?? process.env.PWD ?? process.cwd()`. The handler calls `process.chdir(root)` and uses the resulting `process.cwd()` as the local directory. If that fails: `Failed to change directory to <root>` (`run.ts:167-173`). `directory = options.useServerDirectory ? undefined : (options.directory ?? local)`. Native run always uses `local`.
3. Message: `mergeInput(formatMessage(input.message), process.stdin.isTTY ? undefined : await readStdin())`. stdin is read in full as UTF-8 text (`src/util/io.ts`) only when stdin is **not** a TTY.
   - `formatMessage` (`run.ts:162-165`): every arg that contains a space is wrapped as `"` + arg with `"` replaced by `\"` + `"`. Args are joined with a single space. An empty result becomes `undefined`.
   - `mergeInput(message, piped)` (`run.ts:156-160`): with no message it returns `piped || undefined`; with no piped input it returns `message`; otherwise `message + "\n" + piped`. Tested: `("from args","from stdin")` gives `"from args\nfrom stdin"` (`test/mini.test.ts:110-113`).
4. If `!message?.trim()`, fail with `You must provide a message`.
5. Each `--file` is prepared in parallel (`prepareFile`, `run.ts:185-218`):
   - `path.resolve(root, input)`, opened read-only. On failure: `File not found: <input>` (`<input>` is the original argument).
   - (V1 attached compatibility only) a directory fails with `Cannot attach local directory without a shared filesystem: <input>`.
   - A non-regular file or a file larger than `10 * 1024 * 1024` bytes fails with `Cannot attach a directory, special file, or file larger than 10 MiB: <input>`.
   - The whole file is read. `detected = FSUtil.mimeType(file)`: an extension lookup, else `application/octet-stream`.
   - MIME: if `detected` starts with `image/` or equals `application/pdf`, use `detected`. Otherwise, if the bytes are not binary and round-trip exactly through UTF-8 decode and encode, use `text/plain`. Otherwise use `detected`.
   - `isBinaryContent` (`run.ts:220-224`): empty input is not binary. Any `0x00` byte makes it binary. Otherwise it is binary when the share of bytes `< 9` or in `14..=31` exceeds `0.3`.
   - Result: `{ url: "data:<mime>;base64,<b64>", filename: basename(file), mime }`.
6. `execute(...)`.

#### `execute` (`run.ts:83-154`)

- Creates the client with `baseUrl = endpoint.url` and the Basic auth header when auth is present. Fetch runs with `timeout: false`, so long event streams are never cut off.
- `explicit = parseRunModel(input.model)`, which uses `Model.Ref.parse` (`packages/schema/src/model.ts:26-40`):
  - Splits at the **first** `/`: provider = before it, id = up to the first `#` after it, variant = after that `#`.
  - Throws `Invalid model reference: <input>` if: there is no `/` or it is at index 0; the id is empty; the provider contains `#`; or the variant is present but empty or contains `#`.
  - Example: `openrouter/openai/gpt-5#high` gives provider `openrouter`, id `openai/gpt-5`, variant `high` (`test/mini.test.ts:115-124`).
  - This throw is not a `RunTargetError`, so it is reported through the outer catch.
- `resolveSessionTarget` (below) is called with:
  - `location: {directory}` when a directory is known;
  - `continue`, `session`, `fork`, `model: {providerID, id, variant}` (when explicit), `agent`;
  - `environment: Env.session()` **only if `server.service` is set** (the managed background service). `Env.session()` is all of `process.env` minus `OPENCODE_PASSWORD` and `OPENCODE_SERVER_PASSWORD` (`src/env.ts:15-22`).
  - A `prepare` callback, which returns `{model, agent: next.agent}`. `model = next.model ?? (options.variant ? model.default({location}) : undefined)`, with variant `options.variant ?? selected.variant`. If a variant was requested but no model resolved, it throws `RunTargetError("Cannot select a variant before selecting a model", sessionID)`.
- A `RunTargetError` is reported with its session ID and execution returns. Other errors propagate.
- Title: if the session was **not** resumed and `--title` was given, the handler calls `session.update({ sessionID, title })`. The title is `input.title`, or, when `input.title` is the empty string, `message.slice(0,50) + (message.length > 50 ? "..." : "")`.
- Calls `runNonInteractivePrompt` (next section) with `agent: target.agent`, `model: {providerID, modelID}`, `variant`, `thinking`, `format`, `auto`, `attached: options.attached ?? true` (always `true` for native `run`), `compatibility`, and the `renderTool`/`renderToolError` callbacks bound to `target.location.directory`. Errors are reported through `reportRunError(input, msg, target.session.id)`.

### Session selection (`src/session-target.ts`)

`resolveSessionTarget` (`session-target.ts:32-89`):

1. `selectSession` (`:97-132`):
   - If `--session` is given, the session is fetched with `session.get`. A rejection whose `_tag === "SessionNotFoundError"` counts as "missing"; other errors propagate (`findSession`, `:134-139`).
     - Missing with `--fork`: throws `Error("Session not found")`.
     - Missing without `--fork`: no session is selected, and later a **new session is created with that exact ID**.
     - Found: if `--fork`, `session.fork({sessionID})`, wrapping errors in `SessionTargetMutationError`; otherwise that session is used.
   - Else, if `--continue`: resolve the location (below), then `latestSession`:
     - `session.list({directory, parentID: null, limit: 50, order: "desc", cursor?})`.
     - Returns the first item whose `location.directory === location.directory` **exactly**.
     - Otherwise it follows `cursor.next` until there is no next cursor or a page is empty (`:141-161`). Tested with 50 non-matching sessions, then a match on page 2 (`test/session-target.test.ts:67-78`).
     - With a match and `--fork`, the match is forked. With no match, nothing is selected (a new session is created) and the resolved location is reused.
   - Otherwise nothing is selected.
2. Location: `selection.location`, or `location.get` with the selected session's directory (when a session exists) or the requested location. `location.get()` takes no args when there is no location (`:163-167`).
3. `prepare` receives `{location, session, model: input.model ?? selected?.model, agent: input.agent ?? selected?.agent}`.
4. With no selected session: `session.create({ id: input.session, agent: prepared.agent, model: prepared.model, location: {directory} })`. Errors are wrapped in `SessionTargetMutationError` and **not retried** (`test/session-target.test.ts:132-137`). `prepare` runs before `create` (`:100-121`).
5. If `environment !== undefined`: `session.environment({sessionID, variables})`, with errors wrapped.
6. Returns `{ session, location, model: prepared.model, agent: prepared.agent ?? session.agent, resume: selected !== undefined }`. A fresh session takes its agent from the server's response when none was given (`:123-130`).

`SessionTargetMutationError.message = errorMessage(cause)`. Example: a create returning HTTP 500 yields the message `UnexpectedStatus: 500` (`test/mini.test.ts:203-229`).

`--model` and `--agent` values are passed straight to session creation without any catalog preflight. The CLI never calls `/api/model`, `/api/agent`, or `/api/location/wait` for them (`test/mini.test.ts:152-201`).

### Prompt execution and event streaming (`src/run/noninteractive.ts`)

#### Setup (`noninteractive.ts:76-98`, `:686-760`)

1. Subscribes to `client.event.subscribe` with an abort controller and **awaits the first event**. If the stream ends first: `Event stream disconnected before prompt admission`.
2. `messageID = SessionMessage.ID.create()` (`"msg_" + ascending id`).
3. Registers a SIGINT handler `interrupt` (`:679-686`):
   - On the second SIGINT it calls `process.exit(130)` immediately.
   - On the first it sets `interrupted = true` and `process.exitCode = 130`, aborts the in-flight prompt admission request, and fires `session.interrupt` (errors ignored).
4. If `input.agent` is set (target agent; set in practice whenever the session or server has one): `session.switchAgent({sessionID, agent})`.
5. Model selection: if a model is set, `selected = {providerID, id: modelID, variant}`. Else, if a variant is set (V1 only), `selected` is the session's model or `model.default()`, with that variant. Variant without a model fails with `Cannot select a variant before selecting a model`. If `selected` is set: `session.switchModel({sessionID, model: selected})`.
6. Files are converted (`:848-859`):
   - Non-`text/plain` files become attachments `{uri: dataURL, name: filename}`.
   - `text/plain` files are decoded and inlined as ``<file name="FILENAME">\nCONTENT\n</file>``.
7. If already interrupted, it returns. Otherwise: `submitted = true`, the consume loop starts, then:

```
session.prompt({ sessionID, id: messageID,
  text: [message, ...inlinedFileTexts].join("\n\n"),
  files: attachments, delivery: "steer" }, { signal: admission })
```

   If the prompt request fails: when interrupted, `session.interrupt` is called again. Then the stream is aborted and the loop awaited. If interrupted or an error was already emitted, it returns silently; otherwise it rethrows (reported by `execute` with the session ID).
8. After admission (and again `session.interrupt` if interrupted meanwhile), it fetches in parallel:
   - `permission.list({sessionID})`;
   - `session.form.list({sessionID})`;
   - when not attached, `form.list({location:{directory}})`.

   It then replies to or cancels each one, as in live handling below. Global forms (`sessionID === "global"`) are cancelled only when not attached and the returned `location.directory` matches.
9. V1 compatibility: awaits the consume loop and returns.
10. Native: `waiting = session.wait({sessionID})`, then `await Promise.race([waiting, completed.then(() => waiting)])`. It sets `finalizing = true` and runs `reconcile()`.
    - If no assistant message was projected and there was no interrupt, rejection, cancelled form, emitted error, or pre-promotion error, it awaits the loop.
    - If the prompt message was not found in the projection and none of those flags is set, it emits an error: `prePromotionError ?? {type:"unknown", message:"Prompt was not promoted"}`, sets exit code 1, and emits JSON `error` or calls `UI.error`.
11. `finally`: removes the SIGINT handler and aborts the stream. V1 awaits `stream.return()`; native fires it and continues.

#### Session ownership (`:141-156`)

The run owns its own session and every descendant, determined through `session.get` and its `parentID` chain. Results are cached. Errors mean "not owned". Sessions on the same server that are not owned are ignored. The sentinel session ID `"global"` (used by MCP elicitations) is never owned through the chain.

#### Permission handling (`:158-186`)

This handles `permission.asked` events received after submission for owned sessions, plus any pending permissions after admission.

- `--auto`: reply `decision: "once"`. Nothing is printed.
- Not auto, native (not V1):
  - Stderr: `UI.println("\x1b[93m\x1b[1m!", "\x1b[0mpermission requested: <action> (<resources joined by ", ">); auto-rejecting")`, i.e. `\x1b[93m\x1b[1m! \x1b[0mpermission requested: ACTION (R1, R2); auto-rejecting\n`. This is written to **stderr even in JSON mode**.
  - Reply: `decision: "reject"`, `message:` `This non-interactive run cannot ask the user for permission, so the request was rejected. Continue without this action.`
  - The run continues, because the model sees the rejection.
- Not auto, V1: prints the same line, replies `reject` without a message, sets `permissionRejected = true`, and calls `session.interrupt` on the run session.
- Reply errors are ignored.

#### Form (question/elicitation) handling (`:188-207`, `:225-235`)

This handles `form.created` events after submission when the form belongs to an owned session. It also handles them when not attached, the form session is `"global"`, and `event.location.directory` equals the run location.

- Cancel requests go through `session.form.cancel({sessionID, formID, message?})`. For `"global"` forms, the request carries the header `x-opencode-directory: encodeURIComponent(directory)` (e.g. `%2Fwork%20tree`, `test/run/noninteractive.test.ts:426-433`).
- When `form.metadata.kind === "question"` and the run is not V1, the cancel message is `This non-interactive run cannot ask the user questions, so the question was cancelled. Continue without an answer; make reasonable assumptions and state them.` The run then continues.
- Any other form (or V1): cancels without a message, sets `formCancelled = true`, and (outside V1) `process.exitCode = 1`.
- An error with `_tag === "FormAlreadySettledError"` is ignored. Other errors propagate.
- Nothing is printed for form cancellations.

#### Event loop (`consume`, `:209-548`)

Each event `next()` from the stream:

- Stream error: rethrown unless an error was already emitted (then treated as done).
- Stream end: returns if an error was already emitted, else throws `Event stream disconnected during prompt execution`.
- `permission.asked` and `form.created` are handled first (above), regardless of session.
- Events whose `data.sessionID !== run session` are ignored.
- `time = toMillis(event.created)`: a number is used as-is, an ISO string is parsed, anything else becomes `Date.now()`.
- `session.inbox.delivered` with `inboxID === messageID` sets `promoted = true` and clears `prePromotionError`.
- `session.execution.interrupted` with `reason === "user"` while interrupted, `permissionRejected`, or `formCancelled`: return.
- Before promotion:
  - `session.execution.failed` stores `prePromotionError = data.error` (and returns if finalizing).
  - When finalizing, `succeeded` or `interrupted` returns.
  - Every other event is ignored.
- While finalizing, only `session.execution.*` events are processed.
- `session.step.started` builds part `{id: "prt_" + eventID without leading "evt_", sessionID, messageID: assistantMessageID, type: "step-start", snapshot}`.
  - JSON mode: emits `step_start`.
  - Default mode: `UI.empty(); UI.println("> <agent> · <model.id>"); UI.empty()` on stderr.
  - V1 instead buffers it as `pendingStep` and flushes it before the next text, reasoning, or tool content, a step end, or a step or execution failure.
- `session.text.started` and `session.reasoning.started` record the start part ID and timestamp, keyed by `(assistantMessageID, ordinal)`.
- `session.text.ended` builds part `{id, sessionID, messageID, type:"text", text, time:{start,end}}` and passes it to `writeText`.
- `session.reasoning.ended` is processed only with `--thinking`. Its part adds `metadata: data.state`, and it goes to `writeReasoning`.
- `session.tool.input.started` stores tool state (name, part ID, timestamp).
- `session.tool.input.delta` appends to `raw`; `session.tool.input.ended` sets `raw = data.text`.
- `session.tool.called` updates `input`, `provider: {executed, state}`, and `providerState`. A tool name never seen before becomes `"tool"`.
- `session.tool.progress` sets `metadata`.
- `session.tool.success`: emits `tool_use` (JSON) or calls `renderTool` with a completed tool (default mode).
- `session.tool.failed`: emits `tool_use` (JSON). In default mode:
  - If there is content with non-blank output text, it first calls `renderTool` with that content as a completed tool.
  - Then `renderToolError`, then `UI.error(error.message)`.
  - `metadata` falls back to the last progress metadata, and `content` falls back to the accumulated content when non-empty (`test/run/noninteractive.test.ts:513-549`).
  - V1 with `permissionRejected` or `formCancelled`: the failure is suppressed.
- `session.step.ended`: emits `step_finish` with part `{id, sessionID, messageID, type:"step-finish", reason: data.finish, snapshot, cost, tokens}`. JSON only; default mode prints nothing.
- `session.step.failed`:
  - V1 with message `The provider response ended unexpectedly.` drops `pendingStep`, sets `v1InvalidOutput`, and continues.
  - Ignored if interrupted, rejected, or cancelled.
  - Otherwise: `emittedError = true`, exit code 1, then JSON `error {error: data.error}` or `UI.error(message)`.
- `session.execution.failed` (promoted):
  - V1 returns silently if `v1InvalidOutput`, rejected, or cancelled.
  - Otherwise, if no error has been emitted and no form was cancelled, it emits the error (exit 1). Then it returns.
- `session.execution.interrupted` (promoted):
  - V1 returns if rejected or cancelled.
  - `reason === "user"` while interrupted sets exit code 130.
  - A reason other than `user`, with no error emitted, no rejection, and no cancellation, emits error `{type:"aborted", message:"Session interrupted: <reason>"}` with exit 1.
  - Then it returns.
- `session.execution.succeeded`: return.

#### Reconciliation (`reconcile`, `:550-677`)

- Pages through `message.list`. The first request is `{sessionID, limit: 200, order: "desc"}`; later ones are `{sessionID, limit: 200, cursor}`.
- It collects messages until it reaches `id === messageID`, so only messages newer than the prompt, in chronological order. Not finding the prompt yields `found:false`.
- For each assistant message, with `timestamp = time.completed ?? time.created`:
  - **Text** item number *n* (counted per message): if nothing was rendered for the key, or the full text extends the rendered prefix, only the **unrendered suffix** goes to `writeText`. The part ID is `prt_<messageID without "msg_">_text-<n>` and the time is `{start: message.time.created, end: timestamp}`.
  - **Reasoning** items work the same way (only with `--thinking`), with ID `..._reasoning-<n>` and `metadata: item.state`.
  - **Tools** not yet rendered whose status is not `streaming` or `running`: the part ID is `prt_<id>_tool-<toolCallID>`, and they are emitted or rendered like live tools (completed or error).
  - If `message.error` is set and there is no prior error, rejection, or cancellation: emit it (exit 1).
- Returns `{found, responded: any assistant message}`.

#### Output formats

`UI` helpers (`src/run/ui.ts`) all write to **stderr**:

- `Style`: `TEXT_DIM="\x1b[90m"`, `TEXT_NORMAL="\x1b[0m"`, `TEXT_WARNING_BOLD="\x1b[93m\x1b[1m"`, `TEXT_DANGER_BOLD="\x1b[91m\x1b[1m"`.
- `println(...parts)` writes `parts.join(" ") + EOL` to stderr.
- `empty()` prints `"\x1b[0m" + EOL` **only on the first call in the process**. The module-level `blank` flag is set and never reset, so every later `empty()` is a no-op.
- `error(msg)` strips one leading `Error: `, then prints `"\x1b[91m\x1b[1mError: \x1b[0m" + msg`. Test: stderr contains `Error: \u001b[0mExecution failed` (`test/run/noninteractive.test.ts:486-496`).

**Default format** (`--format default`):

- Text (`writeText`, `:106-117`): trimmed, skipped if empty.
  - When **stdout is not a TTY**: `text + EOL` to **stdout**, no color.
  - When stdout is a TTY: `UI.empty(); UI.println(text); UI.empty()`, i.e. stderr.
- Reasoning (`--thinking` only): the line is `Thinking: <trimmed text>`.
  - Stdout not a TTY: written plain to stdout.
  - TTY: stderr `\x1b[90m\x1b[3mThinking: …\x1b[0m\x1b[0m`, wrapped in `empty()`.
- Step start: stderr `> <agent> · <modelID>` (middle dot U+00B7).
- Step finish: nothing.
- Tool success (`renderTool`, `run.ts:226-240`): `info = toolInlineInfo(part, directory)` (from `packages/tui/src/mini/tool.ts:1158`).
  - Block mode: `empty()`, then stderr `"\x1b[0m<icon> \x1b[0m<title>"`, then `info.body` on its own line if `body.trim()` is non-empty, then `empty()`.
  - Inline mode: stderr `"\x1b[0m<icon> \x1b[0m<title> " + (description ? "\x1b[90m<description>\x1b[0m" : "")`. When there is no description, the line still ends with a trailing space, because the join is over an empty third element.
- Tool failure (`renderToolError`, `run.ts:242-245`): stderr `"\x1b[0m✗ \x1b[0m<title> failed"`, followed by `UI.error(<error message>)`.
- Errors: `UI.error(message)`.

Tool inline table (`packages/tui/src/mini/tool.ts:334-512, 1130-1137`). Names are canonicalized first: `bash` becomes `shell`, `task` becomes `subagent`, `apply_patch` becomes `patch` (`packages/tui/src/util/tool-display.ts:1-6`). Paths are shown relative to the session directory with forward slashes (`toolPath`).

| tool | icon | title | description / mode |
|---|---|---|---|
| shell | `$` | `<command>` | block, body = trimmed output when completed |
| write | `←` | `Write <path>` | block, body = output when completed |
| edit | `←` | `Edit <path>` | block, body = first file patch or `metadata.diff` |
| patch | `%` | `Patch` or `Patch N file(s)` | inline |
| batch | `#` | `Batch` or `Batch N tool(s)` | block, body = output when completed |
| subagent | `✓` (`✗` error, `•` running) | description, or `<Agent> Subagent` | desc `<Agent> Agent` when a description exists; agent is title-cased, default `unknown` |
| question | `→` | `Asked N question(s)` | inline |
| read | `→` | `Read <path>` | desc = `[k=v, ...]` of primitive inputs except `path` |
| glob | `✱` | `Glob "<pattern>"` | `in <path>` and/or `N match(es)` joined by ` · ` |
| grep | `✱` | `Grep "<pattern>"` | `in <path>` and/or `N match(es)` from `metadata.matches` |
| list | `→` | `List <path>` or `List` | inline |
| lsp | `→` | `LSP <op> <file>:<line>:<char>` (`LSP <op>` with no file; op defaults to `request`) | inline |
| webfetch | `%` | `WebFetch <url>` or `WebFetch` | inline |
| websearch | `◈` | `Web Search via <Provider> "<query>"` (or `Web Search …`) | inline |
| skill | `→` | `Skill "<name>"` | inline |
| invalid | `✗` | `Invalid Tool` | block |
| other | `⚙` | `<name> <JSON input>` or `<name> Unknown` | inline |

`count(n, "match")` produces `1 match` and `N matches`. Tool output text (`toolOutputText`, `tool.ts:157-163`):

- `shell` uses only the first text content item.
- `read` unwraps a JSON page envelope.
- Everything else joins text items with `\n`.

**JSON format** (`--format json`) writes one JSON object per line on **stdout** (`emit`, `:100-104`): `{"type", "timestamp", "sessionID", ...data}` + EOL. Undefined fields are omitted, per `JSON.stringify`.

| type | extra fields |
|---|---|
| `step_start` | `part: {id, sessionID, messageID, type:"step-start", snapshot}` |
| `text` | `part: {id, sessionID, messageID, type:"text", text, time:{start,end}}` |
| `reasoning` | `part: {id, sessionID, messageID, type:"reasoning", text, metadata, time:{start,end}}` (only with `--thinking`) |
| `tool_use` | `part` = MiniToolPart (below) |
| `step_finish` | `part: {id, sessionID, messageID, type:"step-finish", reason, snapshot, cost, tokens}` |
| `error` | `error: <server error object {type, message, …}>` |

MiniToolPart for a live **success** (`:408-429`):

```
{ partID, sessionID, messageID, type:"tool", id:<toolCallID>, tool:<name as received>,
  state:{ status:"completed", input, output:toolOutputText(name,content), title:<name>,
          metadata:{ metadata, content, providerCall:{executed,state}, providerResult:{executed,state}, rawInput },
          time:{start,end} } }
```

For a live **failure** (`:457-475`), the state is `{status:"error", input, error:<message string>, metadata:{providerCall, providerResult, rawInput}, time}`. It carries no `output` and no `metadata.metadata` or `metadata.content` (`test/run/noninteractive.test.ts:551-576`). Reconciled tools use `metadata:{metadata, content}` only. JSON success output keeps formatted text, and `metadata.result` is absent (`test/run/noninteractive.test.ts:316-340`).

Pre-session errors (`reportRunError`, `run.ts:248-262`) always set `process.exitCode = 1`.

- JSON: `{"type":"error","timestamp":<Date.now()>,"sessionID":"<id or empty string>","error":{"type":"unknown","message":"<msg>"}}` + `\n`, written to stdout. Example: `UnexpectedStatus: 500` with `sessionID: ""` (`test/mini.test.ts:203-229`).
- Default: `UI.error(msg)` on stderr.

Even in JSON mode, the permission auto-reject warning line is still written to stderr.

#### Exit codes

The process exits with `process.exitCode ?? 0` after the handler (`src/index.ts:139`).

| Code | Cause |
|---|---|
| 0 | success |
| 1 | any `reportRunError`; a step or execution failure; non-user interruption; `Prompt was not promoted`; a non-question form cancelled (native); an unreported top-level failure such as a server connection error |
| 130 | SIGINT (first SIGINT sets it; the second exits immediately with 130); user interruption observed while interrupted |

Two test-pinned edge cases:

- V1 terminal-finish failure `The provider response ended unexpectedly.` exits 0 with no output at all (`test/run/noninteractive.test.ts:498-511`).
- Cancelling a session-owned non-question form (no `metadata.kind === "question"`) in native mode gives exit 1 even though the execution later succeeds (`:440-454`, `:419-438`).

### V1 compatibility bridge (`src/run/v1.ts`)

`runV1Bridge` is exported from `src/run/index.ts`, but no command in this package wires it up (`test/import-boundaries.test.ts:34` only asserts the export). Differences from native `run`:

- Input adds `server`, `password`, `username` (default `opencode`), `directory`, `variant`, `dangerouslySkipPermissions` (mapped to `auto`), and `standaloneCommand`.
- Local mode (no `server`): `chdir(path.resolve(PWD ?? cwd, directory))`. On failure it reports `Failed to change directory to <dir>`. It then starts a standalone server (`Standalone.start({command})`).
- Attached mode passes `directory` through as-is. When it is omitted, the server directory is used.
- Options are `compatibility:"v1"`, `variant`, and `attached`. In V1:
  - step-start is buffered;
  - permission rejects and interrupts;
  - the wait and reconcile steps are skipped;
  - attached mode rejects local directories as attachments.

### `src/ui/timeline.tsx` (TimelineHost)

This utility is not referenced by any command in `packages/cli/src`. Port it as a library for parity.

- API: `intro`, `item`, `pending`, `success`, `failure`, `outro`, `close`, and an abort `signal` that fires on SIGINT, SIGHUP, or SIGQUIT.
- Row markers (`:31-37`):
  - intro `┌` (muted, connector);
  - item `●` (accent, ANSI index 6, connector);
  - success `◇` (green, index 2, connector);
  - failure `■` (red, index 1, no connector);
  - outro `└` (muted, index 8, no connector).
- Non-TTY (stdout or stdin is not a TTY, `:118-159`): writes `"<marker> <text>\n"`, plus `"│\n"` for connector rows. Pending uses marker `.`.
  - Writing after close throws `timeline closed`.
  - Concurrent writes throw `timeline write already in progress`.
- TTY: writes `"\n"`, then opens an opentui split-footer renderer with a footer height of 1.
  - Rows go to scrollback; the connector `│` is shown under connector rows.
  - The footer shows a spinner (frames `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, 80 ms) plus the pending text.
  - Esc or Ctrl+C cancels.
  - success, failure, and outro clear the pending text.
  - Idle waits are bounded to 1 s.

## 10. `stats` command

### Flags (`src/commands/commands.ts:309-339`)

Description: `Show shareable usage statistics`. Accepts `--standalone` and `--server` plus:

| Flag | Type | Constraint | Default | Description |
|---|---|---|---|---|
| `--days N` | int | ≥ 0 | | `Show the last N days; 0 means today` |
| `--year Y` | int | 1970..9999 | | `Show a calendar year` |
| `--all` | bool | | false | `Show lifetime statistics` |
| `--project ID` | string | | | `Filter by project ID, or use "." for the current project` |
| `--models` | bool | | false | `Show model usage` |
| `--tools` | bool | | false | `Show tool reliability` |
| `--cost` | bool | | false | `Show cost and token details` |
| `--full` | bool | | false | `Show every detailed section` |
| `--limit N` | int | ≥ 1 | 5 | `Number of rows in detailed sections` |
| `--json` | bool | | false | `Output statistics as JSON` |

### Logic (`src/commands/handlers/stats.ts:12-70`)

- More than one of `--days`, `--year`, `--all` fails with `--days, --year, and --all cannot be combined`.
- Server: `ServerConnection.resolve({server, standalone})`.
- Range (`statsRange`, `:144-163`), with local-time arithmetic and `to = now + 1` ms:
  - `--all`: `{from: undefined, to, label: "all time"}`.
  - `--days N`: `from` = local midnight today minus `max(0, N-1)` days. The label is `today` when N is 0 or 1, otherwise `last N days`.
  - Year (default: the current year): `from = Jan 1 local`. For the current year, `to = now + 1` and the label is `<year> so far`. For other years, `to = Jan 1 of year+1` and the label is `<year>`.
- `--project .` resolves through `location.get({location:{directory: cwd}})` to `project.id`. Any other value is passed through.
- Request: `session.stats({from, to, project, timezone: Intl resolved timeZone || "UTC", tools})`, with a 30 s timeout. `tools` is:
  - `"detail"` if `--json`, `--tools`, or `--full`;
  - else `"none"` if `--models` or `--cost`;
  - else `"summary"`.
- Transport errors become `Could not reach server at <url>`; other errors are preserved (`stats.ts:72-80`, `test/stats.test.ts:139-157`).
- Output:
  - `--json`: `JSON.stringify(stats, null, 2) + EOL`.
  - Otherwise `renderStats(...) + EOL`, with scope `all projects` when there is no project, `current project` for `.`, and `selected project` otherwise.
  - `color = stdout.isTTY && NO_COLOR is unset`; `width = stdout.columns ?? 80`.
  - The flags `models`, `tools`, `cost` are each OR-ed with `--full`.
- Any failure writes `errorMessage(error) + EOL` to stderr and sets exit code 1.

### Rendering (`renderStats`, `stats.ts:95-142`)

`style(v, code)` is `\x1b[<code>m<v>\x1b[0m` when color is on, else `v`. Primary is `1;<palette.primary>`. Dim is `2`.

Palette (`terminalPalette`, `:330-343`), taken from the last `;`-field of `COLORFGBG`:

- Field ≥ 7 (light background): primary `38;2;59;125;216`; activity levels 1–4 `38;2;153;169;192`, `38;2;122;155;200`, `38;2;90;140;208`, `38;2;59;125;216`.
- Any other finite number (dark): primary `38;2;250;178;131`; activity `38;2;117;99;87`, `38;2;161;125;102`, `38;2;206;152;116`, `38;2;250;178;131`.
- Unset: primary `36`; activity `2;36`, `36`, `1;36`, `1;96`. The test expects `\x1b[1;36m`.

The output is lines joined by EOL, in one of three layouts:

1. **Details mode** (any of models, tools, cost): first line is `style("<label> · <scope>", dim)`. The heading and activity are omitted.
2. **Empty** (`sessions == 0 && prompts == 0 && steps == 0`): heading, `""`, dim `no activity in this range`, `""`, dim `opencode.ai`.
3. **Card**:
   ```
   <heading>
   
   <activity block>
   
   <N session(s)>[ · <N subagent(s)>]        (subagents only when > 0)
   <N prompt(s)> · <N step(s)> · <N token(s)>
   <toolSummary> · <N active day(s)> · best streak <S> day(s)
   
   opencode.ai (dim)
   ```
   - The heading is `style("opencode stats", primary) + " " + style("· <label> · <scope>", dim)`. Test: `opencode stats · 2026 so far · all projects`.
   - Metric counts are `style(formatNumber(v), primary) + " " + noun + (v == 1 ? "" : "s")`.
   - Tokens = `input + output + reasoning + cache.read + cache.write`.
   - `toolSummary` is `tool stats unavailable` (tools mode `none`), `no tool calls` (no finished calls), or `style(formatPercent(succeeded / (succeeded + failed) * 100), primary) + " tool success"`.
   - The streak number is primary-styled; the trailing `day`/`days` is plural unless the streak is 1.

After the base lines, sections are appended in the order **cost, models, tools**. Each is preceded by `""` when lines already exist (always true in practice).

Activity block (`renderActivity`, `:165-194`; calendar from `packages/util/src/activity-calendar.ts`, with `maxWeeks = width - 4`):

- Calendar:
  - Monday-first weeks. Cells are `-1` outside `[from, to)` and `0` when there is no activity.
  - Positive levels come from ranking the distinct positive step counts: `max(1, ceil((rank+1)/count * 4))`.
  - At most 53 weeks, and at most `maxWeeks`, keeping the latest. `clipped` is true when weeks were dropped.
  - Months are labelled by each week's Thursday, clamped into the range, as `en-US` short names in UTC. Repeated labels merge, with a `span` in weeks.
- Line 1: `style(clipped ? "activity · last <weeks> weeks" : "activity", primary)`.
- Line 2: `"   " + style(monthsRow, dim)`. `monthsRow` concatenates each month's label (included only if it is the last month or `label.length <= span`) padded to `span` characters, then right-trimmed. Test: a range from Apr 29 to May 20 shows `    May` with no `Apr` (`test/stats.test.ts:51-63`).
- For each weekday `Mo Tu We Th Fr Sa Su`: `style(label, dim) + " " + cells`. A cell is a space when the level is below 0, else `paintActivity(level)`. A `""` line follows every weekday except `Su`.
- `""`, then `"   " + style("less", dim) + " " + levels 0..4 painted + " " + style("more", dim)`. Plain: `   less ·░▒▓█ more`.
- `paintActivity` glyphs by level: 0 `·`, 1 `░`, 2 `▒`, 3 `▓`, 4 `█`.
  - No color: the bare glyph.
  - Level 0 with color: `\x1b[2m·\x1b[22m`.
  - Otherwise: `\x1b[<activity[level-1]>m<glyph>\x1b[39m`.

Cost section (`:196-209`):

```
COST & TOKENS
  cost                $<cost.toFixed(2)>
  input               <fmt input>
  output              <fmt output>
  reasoning           <fmt reasoning>
  cache read          <fmt cache.read>
  cache write         <fmt cache.write>
  cached input        <formatPercent(cache.read / (input + cache.read + cache.write) * 100, 0 if denom 0)>
```

`row(label, value)` = `"  " + label.padEnd(20) + value`. Test: `cached input        32.3%`.

Models section (`:211-240`):

- No models: `MODELS` / `  no model usage`.
- At most `limit` rows; `more = total - shown`. The model label is `provider/id[#variant]`.
- Width < 68: per model, the label truncated to `width`, then `  <fmt tokens> tokens · <fmt steps> steps · $<cost.toFixed(2)>`.
- Otherwise: a header `tableRow("model","tokens","steps","cost")`, then rows.
- If `more > 0`: `""`, then `+<more en-US> more model(s)`.

Tools section (`:242-277`):

- Mode not `detail`: `TOOL RELIABILITY` / `  tool details unavailable`.
- Empty usage: `  no tool calls`.
- Error column: `-` when there are no finished calls, else `formatPercent(failed / (succeeded + failed) * 100)`. p50 column: `-` or `formatDuration`.
- Compact (width < 68): name, then `  <calls> calls · <err> error · <p50> p50`.
- Wide: header `tableRow("tool","calls","error","p50")`, then rows.
- Then `""`, `<fmt(succeeded+failed)> finished calls · <fmt unfinished> unfinished`, and, if more, `+N more tool(s)` (no blank line before it).

Helpers:

- `tableRow(a, b, c, d)` = `truncate(a, 34).padEnd(34) + b.padStart(10) + c.padStart(12) + d.padStart(12)`.
- `truncate(v, w)` = `v` if `len ≤ w`, else `v.slice(0, w-1) + "…"`.
- `formatNumber`: `≥1e9` gives `<x>b`, `≥1e6` gives `<x>m`, `≥1e3` gives `<x>k`, where x = `toFixed(1)` with a trailing `.0` removed. Otherwise the value is rounded and formatted with `en-US` grouping. Example: 18500 becomes `18.5k`.
- `formatPercent(v)` = `v.toFixed(v >= 10 ? 1 : 2) + "%"`.
- `formatDuration(ms)` = `<round>ms` under 1000, else `<trimDecimal(ms/1000)>s`.

## 11. `upgrade` / `uninstall` / updater

### Flags

- `upgrade` (alias `update`), `src/commands/commands.ts:60-74`. Description: `Upgrade OpenCode to the latest or a specific version`.
  - Optional positional `target`: `Version to upgrade to (with or without a leading v)`.
  - `--method`/`-m`, one of `curl npm pnpm bun yarn vp brew` (`src/services/updater.ts:14`): `Installation method to use`. Any other value is a parse error (non-zero exit, nothing attempted).
- `uninstall`, `commands.ts:75-88`. Description: `Uninstall OpenCode, keeping session data, configuration, and state`.
  - `--dry-run`: `Show what would be removed without removing`.
  - `--force`/`-f`: `Skip confirmation prompts`.

Both commands use `@clack/prompts` 1.0.0-alpha.1 (`intro`, `log.*`, `spinner`, `outro`, `confirm`) for output on stdout. Exact glyphs come from clack: intro `┌`, `log.info` `●`, `log.warn` `▲`, `log.error` `■`, `log.message` `│`, spinner stop `◇` for code 0 and an error glyph for code 1, outro `└`.

`handlePromptErrors` (`src/ui/prompt.ts:23-42`):

- Interrupt: `cancel("Cancelled")`.
- Prompt cancel: `cancel("Cancelled")` and exit code 130.
- Any other error: `log.error(errorMessage(error))`, `outro("Failed")`, exit code 1.

### `upgrade` flow (`src/commands/handlers/upgrade.ts`)

1. `intro("Upgrade")`.
2. The method is `--method`, or `updater.method()`. If neither yields one: `Could not detect the installation method. Pass --method to choose how to upgrade OpenCode.`
3. `log.info("Using method: <method>")`.
4. The target is the positional argument, or `updater.latest(method)`. `version = target.trim()` without a leading `v`.
5. If `version === OPENCODE_VERSION`: `log.warn("OpenCode upgrade skipped: <version> is already installed")`, `outro("Done")`, exit 0.
6. `log.info("From <OPENCODE_VERSION> → <version>")`. Test: `0.0.0-beta-old → 0.0.0-beta-target`.
7. Spinner `Upgrading...`, then `updater.upgrade(method, target)`. On success it stops with `Upgrade complete`; on failure with `Upgrade failed` (code 1). Then `outro("Done")`.

An `UpgradeError` is reformatted before `handlePromptErrors` (`formatUpgradeError`, `:51-81`):

```
<title, whitespace collapsed>

<detail wrapped at max(24, columns-7)>

Command  <command>            (only if command present; preceded by blank line)
Retry    <retry>
```

- All values have VT control characters and `\r` stripped, and are trimmed.
- Field labels are padded to 9 characters. Field values wrap at `max(24, min(61, columns - 9 - 10))`; continuation lines are indented 9 spaces.
- `wrap` splits on `\n` and greedily word-wraps lines longer than the width.

### Updater service (`src/services/updater.ts`)

**Method detection** (`:199-227`):

1. `curl` if `path.resolve(process.execPath) === <home>/.opencode/bin/opencode[.exe]`. `home` is `OPENCODE_TEST_HOME` or the OS home directory.
2. `brew` if `realpath(execPath)` contains `/Cellar/<f>/` for `f` in `opencode` (Homebrew Core), `opencode-beta`, or `opencode-v2` (`anomalyco/tap`).
3. Otherwise `installedPackage` is required:
   - Read `dirname(dirname(realExec))/package.json`.
   - Its `name` must match `^@opencode(?:-ai)?/cli(?:-node)?$`.
   - Some `bin` value must resolve to the real executable.
   - If not, the method is `undefined`.
4. These commands run concurrently, each with a 10 s timeout and a 100 kB output cap; failures count as exit 1 with empty output:
   - `npm list -g --depth=0 <pkg>`
   - `pnpm list -g --depth=0 <pkg>`
   - `bun pm ls -g`
   - `yarn global list`
   - `vp list -g --json <pkg>`
5. The first match in that order wins. A match means stdout contains `<pkg>`, except for `vp`, whose stdout must parse as JSON `[{name}]` with `name === pkg`. A no-match message that repeats the package name is not a match (`test/updater-install.test.ts:300-307`).

**Release lookup** (`release`, `:251-307`), with a 10 s timeout:

- The channel is `OPENCODE_CHANNEL` with `[^a-zA-Z0-9._-]` replaced by `-`. The distribution is `homebrew` for brew, else `npm`.
- URL:
  - Homebrew Core (method brew and formula `opencode`): `https://formulae.brew.sh/api/formula/opencode.json`, version from `versions.stable`, package `opencode`.
  - Otherwise: `https://opencode.ai/update/api/<channel>/<OPENCODE_ARTIFACT>/<distribution>?current=<OPENCODE_VERSION>`, with each segment URI-encoded. Version from `version`, package from `metadata.package`.
  - Example: `https://opencode.ai/update/api/local/cli/npm?current=local`.
  - Build defaults: version, channel `local`, artifact `cli` (`src/version.ts`).
- `UpgradeError` cases:

| Case | Title | Detail | Retry |
|---|---|---|---|
| Fetch error | `Could not check for OpenCode updates` | cause detail | `Check your network, then run opencode upgrade again.` |
| Non-OK status | `Could not check for OpenCode updates` | `The update service returned HTTP <status>.` | `Try again in a few minutes.` |
| Bad JSON | `Could not read the OpenCode update information` | cause detail | `Try again in a few minutes.` |
| Missing version or package | `Could not read the OpenCode update information` | `The update service returned incomplete release information.` | `Try again in a few minutes.` |

- `latest(method?)` = `release(method ?? detected).version`.

**Upgrade** (`upgrade`, `:367-453`):

- The input must pass `parseReleaseVersion`, else plain `Error("Invalid version: <input>")`. The version is the input trimmed without a leading `v`.
- `packageName` is `opencode` for Homebrew Core, else `release(method).package`. `target = <packageName>@<version>`.
- If `installedPackage` differs from `packageName` and the method is pnpm or yarn: `Reinstall <target> with <method> to migrate from <installedPackage>.`
- It holds the file lock `cli-upgrade` (timeout 15 min), then runs:

| method | command |
|---|---|
| npm | `npm install --global [--force] <target>`. `--force` applies when (`OPENCODE_ARTIFACT === "cli"` and the installed package does not end in `/cli-node`) or the installed package differs from `packageName`. |
| pnpm | `pnpm add --global --allow-build=<packageName> <target>` |
| yarn | `yarn global add <target>` |
| vp | `vp install -g --force <target>` if the package was renamed, else `vp update -g <target>` |
| bun | `bun install --global --trust --cache-dir <tmp> <target>`. `<tmp>` is a temp directory `update-*` under the global cache, removed afterwards. The displayed command omits `--cache-dir`. |
| curl | 1) `curl -fsSL -o <tmp>/install https://opencode.ai/v2/install` (displayed as `curl -fsSL https://opencode.ai/v2/install`; title `Could not download the OpenCode installer`; retry `Check your network, then run opencode upgrade again.`). 2) `<bash> <tmp>/install --version <version> --no-modify-path` (displayed as `opencode upgrade <version> --method curl`; title `The OpenCode installer failed`). |
| brew | `brew upgrade <packageName>`. Package is `opencode` (Core) or e.g. `anomalyco/tap/opencode-v2`. |

- Each command has a 5 min timeout and a 100 kB output cap.
- Non-zero exit gives an `UpgradeError`:
  - title `<installName> could not install OpenCode`, where installName is one of `The OpenCode installer`, `npm`, `pnpm`, `Bun`, `Yarn`, `Vite+`, `Homebrew`;
  - detail = concise stderr, else concise stdout, else `The command exited with code <n> without any error output.`;
  - command = the displayed command;
  - retry `Fix the issue above, then run opencode upgrade again.`
- A spawn failure with no stderr or cause gives detail `Failed to update with <method>`.
- `conciseDetail` (`:58-71`): strips VT codes, normalizes `\r` to `\n`, drops blank lines, and keeps the last 12 lines. Beyond 2000 characters it is clipped to `…` + the last 1999 characters. If shortened, it appends `\n\nOutput shortened to the last 12 lines.`
- Other failures get title `Could not prepare the OpenCode upgrade` and retry `Fix the issue above, then run opencode upgrade again.`
- On Windows, the curl installer needs bash. It uses `OPENCODE_GIT_BASH_PATH`, else `<git --exec-path>/../../../bin/bash.exe`. If neither exists: `UpgradeError` with title `Git Bash is required to update OpenCode`, detail `The OpenCode installer runs with Git Bash on Windows, and it was not found.`, and retry `Install Git for Windows or set OPENCODE_GIT_BASH_PATH, then run opencode upgrade again.` On non-Windows it uses `bash`.
- On Windows, for non-brew methods with an owned install, the command runs while holding a `RetainedImage` link (below).

**Removal** (`removal`, `:229-249`). There is none for curl, brew, or an unknown package. Commands (5 min timeout):

- `npm uninstall --global <pkg>`
- `pnpm remove --global <pkg>`
- `bun remove --global <pkg>`
- `yarn global remove <pkg>`
- `vp uninstall -g <pkg>`

Non-zero exit gives `Error(resultDetail)`. On Windows the removal is retained in global tmp.

### Version comparison (`src/services/updater-action.ts`)

- `parseReleaseVersion`:
  - Fails if the input is longer than 256 characters before trimming.
  - Regex: `^v?MAJOR.MINOR.PATCH(-pre(.pre)*)?(+build)?$`.
  - Rejects leading zeros, components above `9007199254740991` (compared as strings by length, then lexically), and numeric prerelease identifiers with a leading zero.
  - Returns `{major, core, prerelease[]}`.
- `action(current, latest, policy)`:
  - `disable` gives `none`.
  - If either version is invalid, or the releases are the same, gives `none`. Same means equal core and equal prerelease identifiers; numeric identifiers are compared after `Number()` coercion; build metadata is ignored.
  - Otherwise returns `policy`. A lower latest version (rollback) still counts (`src/services/updater.test.ts`).

### Auto-update behavior

- Runs only in the default (TUI) command (`src/commands/handlers/default.ts:62-86`): `updater.run` immediately, then every 10 min, serialized by a semaphore.
- `inspect` (`updater.ts:455-483`):
  - Skipped when `OPENCODE_LOCAL` (channel `local`) or when `OPENCODE_DISABLE_AUTOUPDATE` is `1` or `true` (case-insensitive).
  - Policy (`readPolicy`, `:150-158`, `decodePolicy`, `:110-125`): reads `config.json`, `opencode.json`, `opencode.jsonc` from the global config directory (`$XDG_CONFIG_HOME/opencode`, or `OPENCODE_CONFIG_DIR`). The last file yielding a policy wins. The default is `notify`.
  - Policy values: JSONC key `"update": "disable"|"notify"|"auto"`; other values give no policy. The legacy key `"autoupdate"` maps `false` to `disable`, `"notify"` to `notify`, `true` to `auto`.
  - `disable` skips.
- `run`: returns `{available}` under `notify`. Under `auto` it installs through the detected method, returning `{installed}` (or failing with `Installation method not found`). All errors are logged and swallowed.
- `check` is used by TUI `/update`. Under `OPENCODE_LOCAL` it returns `{type:"unavailable", message:"This build runs from a source checkout. Use an installed OpenCode release to check for updates."}`. An invalid latest version gives `Invalid version: <v>`. It returns `available`, or `installed` when an earlier auto-install already happened in this process.
- `apply(version)` installs, or fails with `Installation method not found`.
- Version-mismatch preflight (`src/services/update-preflight.tsx`): only for the TUI default command when the background service is replaced because of a version mismatch, and only when stdin and stdout are TTYs. Otherwise stderr gets `Restarting background server (version mismatch)...` or `Starting background server...` (`default.ts:39-47`).
  - The footer is a 4-row split-footer animation with the monogram.
  - Header: `OpenCode is updating [from <prev>] to <OPENCODE_VERSION>`.
  - Stages: `Keeping your session safe`, `Starting the new background service`, `Loading OpenCode` (minimum 480 ms each), with a progress rail and an `n/3` counter.
  - Success: `OpenCode updated to <v>` and `✓ Ready`.
  - Failure: `OpenCode update paused` and `! <message>`. The failure message is `OpenCode update could not start the new background service`.
  - If the renderer fails to open, it falls back to the stderr text.

### RetainedImage (Windows, `src/services/retained-image.ts`)

- `retain(dir, role)`:
  - Removes stale links matching `^opencode-(?:service|upgrade)-(\d+)-[0-9a-f]+\.exe$` whose PID is dead. A process counts as dead only on `ESRCH` from `kill(pid, 0)`.
  - Hard-links the real executable to `<dir>/opencode-<role>-<pid>-<8 hex>.exe` for the scope, then removes the link. Link failure is logged and ignored.
- `relocate(dir, dest)` renames matching links into `dest`.
- `installed(home)` is false when the executable basename is bun, node, or nodejs. Otherwise it is true if the path contains `node_modules` or equals `<home>/.opencode/bin/opencode.exe`.

### `uninstall` flow (`src/commands/handlers/uninstall.ts`)

1. `intro("Uninstall OpenCode")`. Detects the method and its removal command.
2. Lists background service registrations: files in `global.state` (`$XDG_STATE_HOME/opencode`) matching `^service(?:-.*)?\.json$`.
3. For curl only, shell config files (`shellConfigs`, `uninstall.ts:120-152`):
   - Skipped entirely if the binary's directory contains any other file, since the PATH entry is shared with V1.
   - Candidates are picked by `basename($SHELL || "bash")`, falling back to bash:
     - fish: `~/.config/fish/config.fish`, `$XDG/fish/config.fish`
     - zsh: `$ZDOTDIR|~/.zshrc`, `.zshenv`, `$XDG/zsh/.zshrc`, `$XDG/zsh/.zshenv`
     - bash: `~/.bashrc`, `~/.bash_profile`, `~/.profile`, `$XDG/bash/.bashrc`, `$XDG/bash/.bash_profile`
     - ash and sh: `~/.ashrc`, `~/.profile`
   - `$XDG` is `XDG_CONFIG_HOME` or `~/.config`. Duplicates are removed.
   - Only files that `cleanShellConfig` would change are kept.
4. Output:
   - `log.info("Installation method: <method|unknown>")`
   - `log.message("The following global files will be removed (shared by OpenCode versions and channels):")`
   - `  Cache: <global.cache>`, if it exists
   - per service: `  Stop background service and persistent terminals: <state>/<name>`
   - per shell file: `  Shell PATH: <file>`
   - `  Package: <removal command joined by space>`
   - for curl: `  Binary (manual removal): <execPath>`
   - with no method: `log.warn("Could not detect the installation method. Remove the installation manually after cleanup.")`
5. `--dry-run`: `log.warn("Dry run - no changes made")`, `outro("Done")`.
6. Without `--force`:
   - A non-TTY stdin or stdout fails with `Use --force to uninstall without an interactive terminal, or --dry-run to preview.`
   - `confirm({message:"Are you sure you want to uninstall?", initialValue:false})`. Declining gives `outro("Cancelled")` (exit 0); cancelling gives exit 130.
7. Services: spinner `Stopping background services...`. For each: shut down persistent PTYs (errors ignored), then `Service.stop`. Stops with `Background services stopped`, or `Failed to stop background services` (code 1, and the error propagates).
8. On Windows: relocate retained links from cache to tmp.
9. Spinner `Removing Cache...`. Recursively and forcibly removes `global.cache`; on Windows `Busy` errors are retried every 250 ms, up to 40 times.
   - Success: `Removed Cache`.
   - Failure: `Failed to remove Cache`, and the error `Cache: <msg>` is collected.
10. Each shell file is rewritten with `cleanShellConfig` (`uninstall.ts:154-161`). It drops lines that start (after trimming) with `export PATH=` or `fish_add_path ` and contain `.opencode/bin`, plus a `# opencode` comment line directly before such a line. Errors are collected as `Shell config <file>: <msg>`.
11. Package removal: spinner `Running <cmd>...`. Success: `Package removed`. Failure: `Package manager uninstall failed`, the error is collected, and `log.warn("Run manually: <cmd>")`.
12. curl: `log.message("To finish removing the binary, run:")`, then `log.info("  rm '<execPath with ' escaped as '\\''>'")`.
13. Any collected errors fail with them joined by `\n`, giving `log.error` + `Failed` + exit 1. Otherwise `outro("Done")`.

Data, config, and state directories are **kept**. Only the cache, the service processes, the shell PATH lines, and the package are removed.

## 12. `mini` / `pair` / `reload` / `models` / `api`

### `mini` (`src/commands/handlers/mini.ts`, `src/mini.ts`, `src/mini-host.ts`)

Flags (`commands.ts:340-375`), description `Start the minimal interactive interface`:

| Flag | Type | Alias | Description |
|---|---|---|---|
| `--standalone`, `--server` | (as for `run`) | | |
| `--continue` | bool | `-c` | |
| `--session` | string | `-s` | |
| `--fork` | bool | | `Fork the session when continuing` |
| `--replay` / `--no-replay` | optional bool | | `Restore session history on resume and resize (disable with --no-replay)` |
| `--replay-limit` | int, optional | | `Limit replay to the newest N messages (default: 200)` |
| `--model` | string | `-m` | `Model to use in the format provider/model` |
| `--agent` | string | | |
| `--prompt` | string | | `Prompt to use` |
| `--demo` | bool, hidden | | |

Handler steps:

1. `validateMiniTerminal`: if stdout is not a TTY, it writes `\x1b[91m\x1b[1mError: \x1b[0mopencode mini requires a TTY stdout\n` to stderr and exits with 1 (`src/mini.ts:182-184, 223-226`). This happens **before** the server is resolved (`test/mini.test.ts:244-251`).
2. `ServerConnection.resolve({..., mismatch:"replace"})`.
3. Resolves the TUI config with `terminalSuspend` true except on Windows. `replay` is the flag, else `tui.mini.replay`, else `true`. `replayLimit` is the flag, else `tui.mini.replay_limit`.
4. Calls `runMini` with paths `{home, state, log}`.

`runMini` (`src/mini.ts:33-135`):

- Validation, with the same `fail` format and exit 1:
  - `--replay-limit must be a positive integer`
  - `--fork requires --continue or --session`
- stdin:
  - If stdin is a TTY, it is used as-is.
  - Otherwise piped stdin is read as the initial input, and the controlling terminal is opened (`/dev/tty`, or `CONIN$` on Windows). On failure: `opencode mini requires a controlling terminal for input` (exit 1). The terminal is cleaned up exactly once (`src/mini-host.ts:92-126`).
- Initial input is `mergeInput(piped, prompt)`, which returns `piped + "\n" + prompt` when both are present. The order is reversed relative to `run`.
- `localDirectory` uses PWD; on failure: `Failed to change directory to <root>`.
- An invalid model fails with `--model must use the format provider/model[#variant]`.
- `Session not found` (fork of a missing session) is reported through `fail`. Other errors propagate.
- Session target: as in `run`. The agent is `requestedAgent ?? input.agent`, and the environment is `Env.session()` only when the server has `reconnect` (the managed service).
- `resolveMiniTarget` retries on `ClientError` with reason `Transport`: it calls `reconnect(signal)` repeatedly, sleeping 250 ms between failures, until aborted (`:157-180`).
- The rest is delegated to `@opencode/tui/mini` `runMiniFrontend` (the TUI spec).

Host (`createMiniHost`, `src/mini-host.ts:129-171`):

- Provides `version`, `stdin`, `platform`, stdout writes, `files.readText(fileURL)`, the editor, `paths.home`, and SIGINT/SIGUSR2 subscriptions.
- `startup.showTiming` is true when `OPENCODE_SHOW_TTFD` is `1` or `true`.
- Model variant preferences are stored in `<state>/model.json`.
- When `OPENCODE_DIRECT_TRACE` is set, a JSONL trace is written:
  - file `<log>/direct/<YYYYMMDDTHHMMSSZ>-<pid>.jsonl` with lines `{time, pid, type, data}`; the first is `trace.start`;
  - `<log>/direct/latest.json` holds `{time, pid, cwd, argv, path}`.

### `pair` (`src/commands/handlers/pair.ts`)

Flags (`commands.ts:519-540`), description `Print one-time links to connect a browser or app`:

- `--url <u>`, described as `Use an external HTTP(S) server URL in pairing links`.
  - The value must parse as a URL with protocol `http:` or `https:`, and no username, password, query, or fragment.
  - Otherwise the parse error is `Expected an HTTP(S) server URL without credentials, query parameters, or a fragment`.
  - The value is normalized to `href` with trailing `/` removed.
- `--remote`: `Pair through the OpenTunnel remote address, enabling remote access if needed`.

Flow:

1. If the service config has `disabled === true`: ``Pairing requires the background service; run `opencode service unset disabled` first``.
2. `--remote` with `--url`: `--remote cannot be combined with --url`.
3. `--remote` and remote access not configured: `ServiceConfig.set("remote","true")`. This runs `RemoteTunnel.ensure()`, which prints `Setting up remote access; this can take a minute...` to stderr when the device has no tunnel and times out after 5 min with `Timed out creating the remote access tunnel; run the command again to resume`. It then stops the service and stores a random 16-hex `route`.
4. `Service.ensure(options)` produces the endpoint.
5. URLs:
   - `--remote`: polls `server.info` every 1 s for a URL whose hostname is `<route>.<tunnel hostname>`, where the tunnel hostname comes from OpenTunnel XDG storage profile `default`. Timeout after 3 min: ``Timed out waiting for the remote tunnel; run `opencode pair --remote` again to retry``.
   - `--url`: `[url]`.
   - Otherwise: `server.info().urls`.
6. `pairing = server.pair()` returns `{code, expires_in}`. Each link is `new URL("/auth/connect/<code>", url).href`; the absolute path replaces any base path.
7. Stdout: the lines below, joined by EOL, plus a final EOL.
   ```
   (empty)
     Open a link to connect. Links work once and expire in <round(expires_in/60)> minutes.
   (empty)
     <link>            (one per URL; or "  (no server URLs)")
   [(empty)
     <QR rows>]        (only if any non-loopback URL)
   (empty)
   ```
   - The QR code is `uqr.renderUnicodeCompact(JSON.stringify({code, urls: nonLoopbackBaseURLs}), {border: 2})`, split on `\n`, with each row prefixed by two spaces and rows joined with EOL.
   - Loopback means `localhost`, `*.localhost`, `127.*`, or `[::1]`.
8. Unless `--remote` or `--url` was given, and only when the endpoint hostname is `localhost`, `127.0.0.1`, or `[::1]`, stderr gets:
   ```
     Over SSH? Forward the port, then open the link on your machine:
     ssh -L <port>:<hostname>:<port> <host>
     If port <port> is busy locally, forward another port and use it in the link.
   
     To connect from other devices, run `opencode service set hostname 0.0.0.0`.
   
   ```
   The lines are joined by EOL, plus a final EOL.

### Remote tunnel (`src/services/remote-tunnel.ts`)

- `run({route, target, onURL})` is used by the service server (`src/server-process.ts:177-191`).
  - The target is `127.0.0.1:<port>` for wildcard binds, else the bound address.
  - It connects OpenTunnel routes `{[route]: target}`, then calls `onURL("https://<route>.<tunnel hostname>")`, which adds the URL to `server.info().urls`. On close it calls `onURL(undefined)`.
  - Retries use exponential backoff from 1 s, capped at 30 s. They stop on fatal errors: an attach error, or a message starting with `Certificate issuance failed`. A fatal stop logs ``remote access tunnel stopped; run `opencode service set remote true` to retry``.
- `ensure()`: described under `pair` above.
- `hostname(route)`: `<route>.<identity.hostname>`, or `undefined` when no identity exists.

### Web UI (`src/services/web-ui.ts`)

This is the HTTP transform used by the server (`src/server-process.ts:92`).

- Paths `/api`, `/api/*`, `/auth/*`, and `/openapi.json` go to the API. A route-not-found error becomes an empty 404.
- All other paths are served from embedded assets, **before** authentication.
- `key` is the path without its leading `/`.
  - Brotli: when the key is not `index.html` and `accept-encoding` lists `br` without `q=0`, the brotli bytes are used if present.
  - A missing asset under `_assets/` or `icons/` gives 404 with `cache-control: no-store`.
  - Any other unknown path serves `index.html` (SPA fallback, including `/assets/missing.js` and `/workspace/example.js`). If there is no index, the response is an empty 404.
  - Methods other than GET and HEAD get 405. HEAD returns an empty 200 with the same headers.
- Headers:
  - `content-type` is the mime type of the name.
  - `cache-control` is `no-cache` for `index.html`, `sw.js`, and `registerSW.js`; otherwise `public, max-age=31536000, immutable`.
  - `x-content-type-options: nosniff`.
  - `vary: accept-encoding` for non-HTML assets that have a brotli variant.
  - `content-encoding: br` when brotli bytes are served.
  - CSP:
    ```
    default-src 'self'; script-src 'self' 'wasm-unsafe-eval'[ 'sha256-<hash>']; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:; worker-src 'self' blob:
    ```
    For HTML, the hash is the base64 SHA-256 of the inline `<script id="oc-theme-preload-script">` body (`web-ui.ts:81-92`).
- Tests (`test/web-ui.test.ts`): the shell and assets return 200 with no `www-authenticate` header. API paths return 401 with `Basic realm="Secure Area"` when a password is set. `/auth/connect/<code>` with `accept: text/html` returns 302 and sets a cookie that authorizes `/api/*`.

### `reload` (`src/commands/handlers/reload.ts`)

- Flags: `--standalone`, `--server`. Description `Reload configuration`.
- Calls `client.location.reload()`. On success, stdout gets `Configuration reloaded` + EOL.
- Errors propagate to the top-level reporter (exit 1).

### `models` (`src/commands/handlers/models.ts`)

- Flags: `--standalone`, `--server`. Description `List all available models`.
- Calls `model.list({location:{directory: cwd}})`. Each entry is formatted as `<providerID>/<id>`, sorted with `localeCompare`, and printed one per line followed by EOL. An empty list prints nothing.
- The request is wrapped in `Effect.promise`, so a failure is a defect: the process fails and exits non-zero.

### `api` (`src/commands/handlers/api.ts`)

Flags (`commands.ts:98-114`), description `Make a request to the running server`:

- `--standalone`, `--server`.
- Positional `operation | method path`, 1–2 values: `OpenAPI operation ID, or an HTTP method followed by a path`.
- `-d/--data <body>`: `Request body`.
- `-H/--header <name:value>` (repeatable, at most 100): `Request header in name:value form`.
- `--param key=value`: `OpenAPI path or query parameter`.

Logic:

- Server resolution uses `mismatch:"ignore"`.
- Raw form (`rawRequest`, `api.ts:64-67`): exactly two args, the first in `delete get head options patch post put` (case-insensitive) and the second starting with `/`. The method is uppercased.
- Otherwise, the input must be exactly one arg: `Expected an operation name or an HTTP method and path`.
  - Fetches `GET /openapi.json` with auth. Non-OK: `Failed to load OpenAPI document: HTTP <status>`.
  - Finds the first `(path, method)` with a matching `operationId` (`Operation not found: <id>`).
  - Each `{name}` in the path is replaced with `encodeURIComponent(param)` (`Missing path parameter: <name>`). Unused params become the query string, in `URLSearchParams` order. Example: `/api/session/ses%2Fa?workspace=work` (`src/commands/handlers/api.test.ts`).
- Headers:
  - Start with the auth header.
  - Each `-H` is split at the first `:`. If that index is below 1: `Invalid header, expected name:value: <header>`. Name and value are trimmed, then set (overriding).
  - With `--data` and no `content-type`, `content-type: application/json` is added.
- The request is `fetch(new URL(path, endpoint.url), {method, headers, body})`.
- The response body is written to stdout, with an EOL appended unless it already ends with one. An empty body writes nothing.
- A non-OK response also writes `HTTP <status> <statusText>` + EOL to stderr and sets exit code 1.

## 13. Open questions for sections 9–12

1. **Args after `--` in `run`:** the handler appends `process.argv` after `--` to `input.message`. Is it guaranteed that `effect/cli` never also binds those tokens to the variadic `message` argument? If it did, the text would be duplicated. The test only shows that `--foo` does not trigger `You must provide a message`.
2. **Top-level failure printing:** `src/index.ts:143-149` prints `Cause.pretty(cause)` only when `getErrorReported(Cause.squash(cause))` is truthy. The exact stderr text for unhandled handler failures (server connection errors in `run`, `reload`, `models`, `api`, `pair`) depends on Effect runtime internals and should be pinned down in the general CLI or runtime spec.
3. **`UI.empty()` is once per process:** `blank` is never reset. Only the first blank-line call ever prints `\x1b[0m\n`. This looks unintentional, but the replica should match it exactly.
4. **Text destination in `run` default mode:** with a TTY stdout, assistant text goes to **stderr**; only a non-TTY stdout gets plain text on stdout. Confirm this is the intended behavior to replicate.
5. **Clack rendering:** the exact `@clack/prompts@1.0.0-alpha.1` glyphs, spacing, and colors for `intro`, `log.*`, `spinner`, and `outro` (used by `upgrade` and `uninstall`) are library-defined and were not verified here. They need a dedicated reference capture.
6. **Renamed-package migration in `upgrade`:** the `Reinstall <target> with <method> to migrate from <pkg>.` error applies only to pnpm and yarn. bun, npm, and vp proceed: npm adds `--force`, vp uses `install --force`, and bun just installs. Confirm that bun's lack of special handling is intended.
7. **`src/ui/timeline.tsx`:** it has no consumers in `packages/cli`. Is it dead code, or used by a planned command? Port it only if needed.
8. **`runV1Bridge`:** it has no in-repo caller. Confirm whether the Rust port needs the V1 compatibility path at all.
9. **Out-of-scope renderers:** `toolInlineInfo`, `normalizeInput`/`normalizeMetadata`, and `Locale.titlecase`/`duration` live in `packages/tui`. The tool rendering table above summarizes them; full fidelity requires the TUI mini tool spec.

---

## 14. ACP (Agent Client Protocol) server

Source citations: `acp/X.ts:N` means `packages/cli/src/acp/X.ts` line N; `handler/acp.ts:N` means
`packages/cli/src/commands/handlers/acp.ts`; tests are `test/acp/X.ts` under `packages/cli/test/acp/`.
The TS implementation uses `@agentclientprotocol/sdk@1.6.0` (`packages/cli/package.json:27`) for JSON-RPC framing,
so some wire details come from the SDK, not from opencode code. Those are flagged under "Open questions".

### Module map

| File | Role |
|---|---|
| `commands/handlers/acp.ts` | CLI entry: starts the private server, builds the HTTP client, connects ACP over stdio, decides the exit code |
| `acp/agent.ts` | Registers JSON-RPC method handlers and maps errors at the handler boundary |
| `acp/connection.ts` | Outbound calls to the client (`session/update`, `session/request_permission`, `elicitation/create`, extension notifications) and "response written" tracking |
| `acp/service.ts` | Implements each ACP method |
| `acp/sessions.ts` | Registry of attached sessions, MCP registration, pushing `available_commands_update` and `config_option_update` |
| `acp/catalog.ts` | Per-cwd catalog of models, modes (agents) and commands, with reload on server events |
| `acp/config-option.ts` | Builds config options and resolves `set_config_option` values |
| `acp/turn.ts` | `session/prompt` turn engine: event subscription, asks queue, cancel and wind-down, background children |
| `acp/translate.ts` | Pure fold from opencode events to ACP outputs, plus the stop reason and the error result |
| `acp/tool.ts` | Tool-call shapes (kind, title, locations, rawInput, content) |
| `acp/permission.ts` | Permission request mapping and diff previews |
| `acp/elicitation.ts` | Maps forms to ACP form elicitation |
| `acp/compaction.ts` | Compaction lifecycle mapping (standard updates vs `_meta` marker) |
| `acp/replay.ts` | History replay for `session/load` |
| `acp/content.ts` / `acp/prompt.ts` | Converts ACP prompt content to opencode parts, and replay parts back to ACP content |
| `acp/directories.ts` | Additional directories, stored as permission rules and session metadata |
| `acp/child.ts` | Child (subagent) session projection and the extension notification |
| `acp/capabilities.ts` | Parses client capabilities |
| `acp/client.ts` | Classifies opencode client errors and decodes session IDs |
| `acp/error.ts` | Error types and their JSON-RPC mapping |

### Command startup

CLI spec (`packages/cli/src/commands/commands.ts:89-97`):
- Command `acp`, description `"Start an Agent Client Protocol server"`.
- Flag `--login` (boolean, default `false`), description `"Run auth login instead of starting the server"`.

Handler (`handler/acp.ts:13-61`):
1. If `--login` is set, run the `auth login` handler with `target: none, method: none, answer: [], server: none, standalone: false`, then return (`:16-25`). No ACP server starts.
2. Set env `OPENCODE_CLIENT=acp` (`:26`).
3. Start a private standalone server: `Standalone.start()` (`:27`; `packages/cli/src/services/standalone.ts`):
   - Spawn the CLI's own executable (`selfCommand()`) with args `serve --stdio --port 0` (`standalone.ts:17-20`).
   - cwd is the process's startup directory. env is the inherited environment plus `OPENCODE_PASSWORD=<random 32 bytes, base64url>`, and this entry overrides any inherited value (`:21-25`, `:37`).
   - The child's stdin is a pipe. EOF on that pipe ends the server's ownership lease. stderr is inherited only when `OPENCODE_PRINT_LOGS=1`, otherwise ignored. Kill signal is SIGTERM, then a force kill after 3 s (`:26-31`).
   - Read child stdout line by line. The first line is JSON `{"url": string}`. Keep draining stdout after that (`:41-50`). If stdout ends before a line arrives, fail with `"Standalone server exited before reporting readiness"`.
   - Endpoint auth is `basic`, user `opencode`, password as above (`:53`).
4. Build the opencode HTTP client with `baseUrl = endpoint.url`. Every request sends `authorization: Basic base64("opencode:" + password)` (`handler/acp.ts:28-36`; `packages/client/src/service-probe.ts:93-98`).
5. `ACP.connect(client, ndJsonStream(stdout, stdin))` (`handler/acp.ts:37`). Transport is newline-delimited JSON-RPC 2.0: read from stdin, write to stdout.
6. Race two things (`:38-54`):
   - The connection closes (stdin EOF). If the close reason is the SDK's `MessageTooLargeError`, the failure is `incoming message exceeded the ${maxMessageBytes/1024/1024} MiB limit`. Otherwise there is no failure.
   - The server child exits. Failure is `server exited unexpectedly (${reason})`, where reason is `code <n>`, `signal <SIG>` (killed by a signal), or the error message.
7. If there is a failure, write `opencode acp: ${failure}\n` to stderr and exit with code 1. Otherwise exit with code 0 (`:56-59`). The process exits directly and does not wait for the server's graceful shutdown. Closing the lease pipe ends the server. Tests: `test/acp/lifecycle.subprocess.test.ts:5-37` checks that stdin EOF gives exit 0, and that a SIGKILL of the server gives exit 1 with stderr `opencode acp: server exited unexpectedly (signal SIGKILL)`.

#### Connection setup (`acp/agent.ts:26-130`)
- The SDK agent app is named `"opencode"` (`:58`).
- Handlers are registered before the services are built. Each handler first awaits a `ready` deferred, so early requests queue rather than fail (`:29-33`, `:41`).
- At construction:
  - `ACPCatalog.make(client)` opens one SSE event subscription (`:28`).
  - `ACPSessions.make` opens a second SSE subscription (`:122`).
  - `ACPTurn.make` (`:124`).
  - Capabilities start as `parse(undefined)`, which sets all flags false (`:123`).
- Span names are `cli.acp.<method with "/" replaced by ".">` (`:132`).
- Registered requests (`:68-115`): `initialize`, `authenticate`, `session/new`, `session/load`, `session/list`, `session/delete`, `session/resume`, `session/close`, `session/fork`, `session/set_config_option`, `session/set_mode`, `session/prompt`.
- Registered notification: `session/cancel` (`:116-119`).
- Any other method is handled by the SDK (presumably JSON-RPC method-not-found).
- **No `session/set_model` handler.** The model is changed only through `session/set_config_option`.

#### "Responded" gate (`acp/connection.ts:16-70`, `acp/agent.ts:42-45`)
- For each request that has an id, a deferred is registered under that id.
- When the agent writes the JSON-RPC response for that id:
  - A `result` response resolves the deferred.
  - An `error` response interrupts it.
- Notification handlers get a no-op gate.
- `sessions.attach` uses this gate so that `available_commands_update` (and later `config_option_update`) is sent only after the response to `session/new`, `load`, `resume` or `fork` has been written. If that response was an error, these updates are never sent.
- Order verified by test (`test/acp/session.test.ts:115-136`):
  - new, resume, fork: `["response","available_commands_update"]`
  - load: `["user_message_chunk"(replay)…, "response", "available_commands_update"]`

#### Outbound client calls (`acp/connection.ts:53-62`)
- `session/update` notification: `{sessionId, update}`.
- `session/request_permission` request. Cancellable: when the ask is abandoned, the SDK sends `$/cancel_request` with `{requestId}` (verified `test/acp/permission.test.ts:138-142`).
- `elicitation/create` request, cancellable the same way.
- Extension notification `opencode/session/child_update` (`acp/child.ts:3`).
- An SDK `RequestError` from the client is a normal failure. Any other throw is a defect.

### Advertised capabilities: `initialize` (`acp/service.ts:106-131`)

1. Store parsed client capabilities (`acp/capabilities.ts:7-15`). They are global for the connection, and the last `initialize` wins:
   - `childSessionUpdates = clientCapabilities._meta["opencode/child-session-updates"] === true`
   - `formElicitation = clientCapabilities.elicitation.form` is neither undefined nor null
   - `compaction = clientCapabilities.session.compaction` is neither undefined nor null. An explicit `null` counts as unsupported (test `test/acp/compaction.test.ts:36`).
2. Response. The protocol version is always 1 and is not negotiated: a request for 99 still gets 1 (`test/acp/session.test.ts:49`).
```json
{
  "protocolVersion": 1,
  "agentCapabilities": {
    "loadSession": true,
    "mcpCapabilities": { "http": true, "sse": false },
    "promptCapabilities": { "embeddedContext": true, "image": true },
    "sessionCapabilities": { "additionalDirectories": {}, "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {} },
    "_meta": { "opencode/child-session-updates": true }
  },
  "authMethods": [ <authMethod> ],
  "agentInfo": { "name": "OpenCode", "version": "<OPENCODE_VERSION>" }
}
```
3. `authMethod`, base form (`:108-112`):
   `{"description":"Run `opencode auth login` in the terminal","name":"Login with opencode","id":"opencode-login"}`.
   - If `clientCapabilities.auth.terminal` is truthy, add `"type":"terminal","args":["--login"]`.
   - If `clientCapabilities._meta["terminal-auth"] === true`, add
     `"_meta":{"terminal-auth":{"command":"opencode","args":["auth","login"],"label":"OpenCode Login"}}` (`:114-118`).

### `authenticate` (`acp/service.ts:132-136`)
- If `methodId !== "opencode-login"`, fail with `UnknownAuthMethodError`: -32602, data `{methodId}`, message detail `unknown auth method: <id>`.
- Otherwise return `{}`. Nothing else happens. Provider auth is detected per prompt (see the `AuthRequired` mapping).

### Session methods

Shared helpers:
- **`supportedMcpServers`** (`acp/service.ts:243-253`). Accept servers that have no `type` (stdio) or `type === "http"`. Any `sse` or `acp` server rejects the whole request with `InvalidRequestError("Only stdio and HTTP MCP servers are supported", field "mcpServers")`. This runs before any server call (test `test/acp/session.test.ts:186-222`).
- **`getSession(sessionId, cwd)`** (`acp/service.ts:97-103`):
  - Decode the session ID with the `Session.ID` schema. Failure gives `InvalidRequestError("Invalid session ID", field "sessionID")` (`acp/client.ts:26-30`).
  - `GET /api/session/:id`. A server `SessionNotFoundError` becomes ACP `SessionNotFoundError`.
  - If `FSUtil.resolve(cwd) !== FSUtil.resolve(session.location.directory)`, fail with `SessionDirectoryMismatchError{sessionId,cwd}`. `FSUtil.resolve` is `path.resolve` then `realpath`, falling back to the resolved path on ENOENT or ENOTDIR.
- **Directories:** see the "Additional directories" section.

#### `session/new` (`acp/service.ts:137-147`)
1. `ACPDirectories.parse(cwd, additionalDirectories)` validates cwd and the directories.
2. `supportedMcpServers(mcpServers)`.
3. `catalog.get(cwd)`. This runs first so that a catalog failure leaves no session behind.
4. `client.session.create` with `{location:{directory: cwd}, ...grant(directories)}`, where the grant adds `permissions` and `metadata` only if there are directories. Wire: `POST /api/session` with body `{location:{directory}, permissions?, metadata?}`.
5. `sessions.attach(created, cwd, mcpServers)`.
6. Return `{sessionId, configOptions}`.

#### `session/load` (`acp/service.ts:148-164`)
1. Parse directories.
2. Check MCP servers.
3. `getSession`.
4. `ACPDirectories.activate`.
5. `attach(session, session.location.directory, mcp)`.
6. `ACPReplay.history(...)` sends all history as `session/update`s before the response.
7. Return `{configOptions}`, computed from the catalog and the selection after replay.
8. If replay or option building fails, release the attachment (`sessions.release`).

#### `session/resume` (`acp/service.ts:203-210`)
- Same as load, but with no replay.
- Returns `{configOptions}`.

#### `session/fork` (`acp/service.ts:216-225`)
1. Parse directories and check MCP servers.
2. `getSession` on the source. This applies the cwd check, so a mismatch creates no fork (test `test/acp/session.test.ts:82-86`).
3. `POST /api/session/:id/fork`.
4. `activate(forked, directories)`. This replaces the grants inherited from the source with the ones in this request.
5. Attach the fork.
6. Return `{sessionId: forked.id, configOptions}`. No replay.

#### `session/list` (`acp/service.ts:165-192`)
- Request: `GET /api/session` with:
  - `directory = parseCwd(cwd)`, only if `cwd` is neither undefined nor null. A relative or empty cwd gives `InvalidRequestError("cwd must be an absolute path: <cwd>", "cwd")`.
  - `order: "desc"`, `limit: 100`, and `cursor` if `params.cursor` is truthy (an opaque string).
- Response:
  - `sessions[]` items: `{sessionId: s.id, cwd: s.location.directory, additionalDirectories?, title, updatedAt}`.
    - `additionalDirectories` is present only if the session metadata list is non-empty.
    - `title = s.title ?? "${parentID ? "Child" : "New"} session - <ISO(time.created)>"` (`packages/util/src/session-title-fallback.ts:14-16,34-36`).
    - `updatedAt = ISO-8601 of time.updated`.
  - `nextCursor` only if `page.cursor.next`.
- A server `InvalidCursorError` becomes `InvalidRequestError(message, field "cursor")` (`acp/client.ts:21-22`).

#### `session/delete` (`acp/service.ts:193-202`)
1. `turn.cancel({sessionId})` interrupts and awaits any active turn.
2. Decode the id, then `DELETE /api/session/:id`. An invalid id or `SessionNotFoundError` is ignored. Other errors are classified.
3. `sessions.detach(id)`.
4. Return `{}`.

#### `session/close` (`acp/service.ts:211-215`, `acp/turn.ts:397-404`)
1. If a turn is active, interrupt it and await the cancelled result. The turn's wind-down already calls server interrupt.
2. Otherwise call `POST /api/session/:id/interrupt` directly, ignoring an invalid id or not-found.
3. `detach`.
4. Return `{}`.

Test `test/acp/prompt.test.ts:508-552` checks that the prompt response is written before the close or delete response. After that, prompting the closed session gives -32602 with `{sessionId}`.

#### `session/set_config_option` (`acp/service.ts:226-232`)
1. `sessions.require(id)`. If not attached, fail with `SessionNotFoundError`.
2. If `value` is not a string (for example a boolean), fail with `InvalidConfigOptionError{configId}`.
3. `withReload(select(attached, configId, value))`:
   - On `InvalidModel`, `InvalidMode` or `InvalidEffort`, reload the catalog for cwd once and retry (`:80-85`).
4. Return `{configOptions}` from the current state.

#### `session/set_mode` (`acp/service.ts:233-237`)
- `select(attached, "mode", modeId)` with the same reload-retry.
- Returns `{}`.

#### `select` (`acp/service.ts:87-95`, `acp/sessions.ts:184-191`)
1. `resolveChange` produces a change.
2. The attached selection ref is updated **before** the server call, so the echoed `session.*.selected` event is a no-op.
3. Server call:
   - model change: `POST /api/session/:id/model` body `{model:{providerID,id,variant?}}`
   - mode change: `POST /api/session/:id/agent` body `{agent}`

### Session registry and attach (`acp/sessions.ts`)
- **Selection-event subscription** (`:54-73`):
  - One SSE subscription for the whole service.
  - `server.connected` resolves a `connected` deferred. That deferred also resolves if the stream ends.
  - `session.model.selected` events become patch `{model: data.model}`, and `session.agent.selected` events become `{modeID: data.agent}`. Both go to the entry's queue for `data.sessionID`.
  - If the stream fails, log a warning `"ACP selection event stream failed"`.
- **`attach(session, cwd, mcpServers)`** (`:124-164`):
  1. Await `connected`.
  2. Get `current = catalog.get(cwd)`.
  3. Create an entry: `{id, cwd, selection: {model: session.model, modeID: session.agent}}`, a child scope, and an unbounded queue.
  4. Replace synchronously any existing entry for that id, and close the old entry's scope.
  5. `registerMcp`. If it fails, remove the entry and propagate the error.
  6. Fork into the entry scope:
     1. Await the responded gate.
     2. Send `{sessionUpdate:"available_commands_update", availableCommands}`.
     3. Fold over the merge of `catalog.changes(cwd)` (current value first, then each reload) and the selection patches. On each step, run `changed(previous, next, patch)` (`:81-91`):
        - Update the selection with the patch.
        - If `configOptions(next, newSel)` deep-differs from `configOptions(previous, oldSel)`, send `{sessionUpdate:"config_option_update", configOptions}`.
        - If `next.commands` deep-differs from `previous.commands`, send `available_commands_update` again.
        - Errors are ignored.
  7. Return `{attached, configOptions: configOptions(current, selection)}`.
- **MCP registration** (`:93-112`, `:195-209`):
  - The registry is a per-session map `name -> config`. It outlives re-attaches and is deleted on remove or detach.
  - A server is re-registered only when its config deep-differs from the registered one. Registrations run concurrently. On failure, the registry entry is removed.
  - Wire: `client.mcp.add({server: name, location:{directory: attached.cwd}, config})`, which is `PUT /api/experimental/mcp/:name?location[directory]=<cwd>` with body `{config}`.
  - stdio config: `{type:"local", command:[command, ...args], environment: Object.fromEntries(env name→value)}`.
  - http config: `{type:"remote", url, headers: Object.fromEntries(headers name→value), oauth:false}`.
  - Test `test/acp/session.test.ts:138-184`: a duplicate in one request registers once. Resume with an unchanged config registers nothing. A changed config registers again.
- **`detach(id)`**: remove the entry and its MCP registry, and close its scope. This stops the fold and any background child follower.
- **`release(attached)`**: the same, but only if the entry still holds this exact `attached`.
- **`require(id)`**: return the entry, or fail with `SessionNotFoundError{sessionId}`.

### Catalog (`acp/catalog.ts`)
- **Data**: `{models (enabled only), defaultModel: {providerID,id,variant?}, modes: [{id,name,description}], defaultModeID, commands (excluding builtins)}`.
- **Builtin commands**: `compact` with description `"Compact the session"`, start `"compaction"` (`:9-11`).
- **Cache key**: `FSUtil.resolve(cwd)`. One in-flight load is shared. A failed load is evicted (`:98-113`).
- **Load** (`:127-137`):
  1. Poll `GET /api/plugin?location[directory]=cwd` every 25 ms, for up to 5 s, until `data.length > 0`. Errors and timeout are ignored.
  2. `read`, retrying every 25 ms for up to 5 s while the error is `CatalogNotReadyError`.
- **Read** (`:139-170`):
  1. Concurrently call `GET /api/model`, `/api/model/default`, `/api/agent` and `/api/command`, each with `location[directory]=cwd`. Any error becomes `CatalogLoadError{cause}`.
  2. `models = data.filter(enabled)`.
  3. `defaultModel`:
     - If the default endpoint returned a model, look it up in `models`. If it is missing, fail with `CatalogNotReady("models")`.
     - Otherwise use `models[0]`. If there is none, fail with `CatalogNotReady("models")`.
     - Its `variant` is `"default"` if the model has a variant with id `"default"`, otherwise undefined.
  4. `agents = data.filter(mode !== "subagent" && !hidden)`. The first agent is the default mode. If there are none, fail with `CatalogNotReady("agents")`.
  5. `modes = agents.map({id,name,description})`.
  6. `commands = data.filter(name not in builtins)`.
- **Reload triggers** (`:39`, `:68-85`):
  - Events `model.updated`, `agent.updated` and `command.updated` reload the entry for `FSUtil.resolve(event.location.directory)`, or every entry when the event has no location.
  - The first `server.connected` gates entry creation.
  - Reloads are coalesced with requested and loaded generation counters under a 1-permit lock. Reload errors are ignored and keep the old value.
- **Error mapping at the handler** (`acp/agent.ts:47-51`):
  - `CatalogLoadError` is classified by its cause: a transport error becomes ServerUnavailable, anything else is a defect.
  - `CatalogNotReadyError` becomes `ServiceFailureError{safeMessage, errorName:"CatalogNotReady"}`, with safeMessage `"No models are available"` or `"No primary agents are available"` (`acp/error.ts:58-64`).
- **Available commands** (`acp/config-option.ts:70-75`): catalog commands `{name, description: description ?? ""}` followed by builtins `{name:"compact", description:"Compact the session"}`.

### Config options (`acp/config-option.ts`)
`configOptions(catalog, selection)` (`:22-68`), with `model = selection.model ?? catalog.defaultModel`. It returns these options in this order:

1. **model**
   ```json
   {"id":"model","name":"Model","category":"model","type":"select",
    "currentValue":"<providerID>/<modelID>",
    "options":[{"value":"<providerID>/<id>","name":"<providerID>/<model.name>"}, ...]}
   ```
   - Options are sorted by `providerID` (code-unit string order), then by `name.localeCompare`.
   - `currentValue` never includes the variant.
2. **effort**, present only if the current model (found by providerID and id) has at least one variant:
   ```json
   {"id":"effort","name":"Effort","description":"Available effort levels for this model","category":"thought_level","type":"select",
    "currentValue":<selectVariant>,"options":[...]}
   ```
   - Option values: unique `[...variantIDs, "default"]`. Insertion order is kept, so `"default"` comes last unless it is already a variant.
   - Option name: split the value on `[_-]`, uppercase the first character of each part, and join with a space. For example `"default"` becomes `"Default"`, `"x-high"` becomes `"X High"`.
   - `selectVariant(v, variants)` (`:138-143`):
     - If `v` is empty or `"default"`, return `"default"`.
     - If `v` is in `variants`, return `v`.
     - Else, if `"default"` is in `variants`, return `"default"`.
     - Else, return `variants[0]`.
3. **mode**
   ```json
   {"id":"mode","name":"Session Mode","category":"mode","type":"select",
    "currentValue":"<selection.modeID ?? catalog.defaultModeID>",
    "options":[{"value":agent.id,"name":agent.name,"description"?:agent.description(only if truthy)}]}
   ```

`resolveChange(catalog, selection, configId, value)` (`:77-101`):
- **`"model"`**: `requireModel`.
  - Parse the value (`parseModelSelection`, `:103-116`):
    - An exact match of `provider/id` against the advertised values gives `{providerID,id}`.
    - Else split at the last `/`. If the prefix matches an advertised model that has a variant equal to the suffix, the result is `{providerID,id,variant}`.
    - Else split at the first `/` to get the provider and the rest as the id. With no `/`, the result is `{providerID: value, id: ""}`.
  - Look the model up. If not found, fail with `InvalidModelError{providerId, modelId: <raw value>}`.
  - If a variant was parsed but does not exist, fail with `InvalidEffortError{effort}`.
  - Resulting variant:
    - The parsed variant, if any.
    - Else the current variant, if the current model is the same provider and id and the current variant is `"default"` or a valid variant.
    - Else undefined.
- **`"effort"`**:
  - If the current model is not found, or the value is neither `"default"` nor a variant id, fail with `InvalidEffortError{effort: value}`.
  - Otherwise the result is the model `{...current, variant: value}`. The literal `"default"` is sent as the variant.
- **`"mode"`**: the value must equal some `modes[].id`, otherwise fail with `InvalidModeError{mode}`. The result is `{modeID}`.
- **Anything else**: `InvalidConfigOptionError{configId}`.

### `session/prompt` and the turn engine (`acp/turn.ts`, `acp/translate.ts`, `acp/prompt.ts`)

#### Admission and concurrency (`acp/turn.ts:374-393`)
- There is at most one active prompt per sessionId, tracked in a FiberMap.
- A second concurrent prompt fails synchronously with `ServiceFailureError{safeMessage:"Session already has an active ACP prompt: <id>", service:"session"}`, which is -32603.
- The turn runs as an **uninterruptible** fork. It can be interrupted only inside `execute` and at a final check in `settle`, so a response is always produced.
- If the SDK aborts the request's signal (client `$/cancel_request` for this prompt id), the turn is interrupted exactly as with `session/cancel`. The request does not fail: it returns `stopReason:"cancelled"` (test `test/acp/prompt.test.ts:495-506`).
- `session/cancel` (notification): remove and interrupt the session's fiber if present. Unknown sessions are silently ignored (`:394-396`).

#### Run (`acp/turn.ts:352-372`)
1. `require(sessionId)`. If not attached, fail with -32602 data `{sessionId}`.
2. `prepare(catalog.get(cwd), params.prompt)`.
3. Snapshot the capabilities.
4. **open** (`:70-90`):
   - Open a new SSE subscription (`GET /api/event`) for this turn. It is buffered unboundedly and parented to the service scope.
   - Start an ask-queue worker that runs queued asks **one at a time** in order. Failures are logged as `"ACP ask reply failed"`.
5. **execute** (`:281-293`):
   1. The first event must be `server.connected`. Otherwise this is a defect `expected server.connected, got <type>`, which becomes an internal error.
   2. Fork `consume`, which loops `advance` until a terminal state, then waits for all queued asks to settle.
   3. `submit`.
   4. If the prompt was a catalog slash command, return `"succeeded"` immediately: the command does not wait for events, and the consumer is interrupted. Otherwise join the consumer.
   5. If interrupted, run `windDown`.
6. **handoff** (`:295-312`). On success or interrupt-only exit, if `state.openChildren` is non-empty, keep following the children in the background. Otherwise close the subscription.
7. **settle** (`:314-326`):
   - On a non-interrupt failure, re-raise it.
   - `terminal` is the success value, or `"interrupted"`.
   - If not interrupted and `failure(state)` is defined, fail with that error.
   - `sendUsageUpdate`.
   - If an interrupt arrived during settling, terminal becomes `"interrupted"`.
   - Return `response(state, sessionId, terminal)`.

#### Prompt preparation (`acp/prompt.ts:21-59`)
1. If any `image` block has falsy `data` **and** falsy `uri`, fail with `InvalidRequestError("image content has no data or uri", field "prompt")`, which is -32602. The detail is `Invalid params: image content has no data or uri` (test `test/acp/prompt.test.ts:324-331`).
2. Convert with `promptContentToParts` (see "Prompt content conversion").
3. Check readability of every `file://` file part with `access(path, R_OK)`. A directory passes. An unreadable path is replaced in place by the text part `linkReference(filename, url)`, that is `[<filename>](<url>)`.
4. `visible` = parts that are not text, plus text parts that are neither `synthetic` nor `ignored`.
5. `text` = the visible texts joined with `"\n"`.
6. `files` = the visible file parts, as `{uri: url, name: filename}`.
7. `synthetic` = the texts of the synthetic parts.
8. Slash detection on `text.trim()`:
   - It must start with `/`.
   - `name` is the first whitespace-separated token after `/`, and must be non-empty.
   - `args` is the remaining tokens joined with `" "`, then trimmed.
9. `start`:
   - `{type:"compaction", id}` if the slash name is the builtin `compact`.
   - Otherwise `{type:"input", id}`.
   - `id = SessionMessage.ID.create()`, which is `"msg_" + ascending id`.
10. `command` = the catalog command whose name equals the slash name. Builtins never match because they are excluded from the catalog.

#### Submit (`acp/turn.ts:226-259`)
1. If `synthetic` is non-empty, `POST /api/session/:id/synthetic` with body
   `{text: synthetic.join("\n\n"), description:"ACP embedded context", delivery:"steer", resume:false}`.
2. Then one of:
   - Compaction: `POST /api/session/:id/compact` with body `{id}`.
   - Catalog command: `POST /api/session/:id/command` with body `{name, text: slash.args ?? "", files, delivery:"steer"}`.
   - Otherwise: `POST /api/session/:id/prompt` with body `{id, text, files, delivery:"steer"}`.

Tests: `test/acp/prompt.test.ts:254-277` covers the command and compact submissions. `test/acp/prompt.test.ts:279-332` covers synthetic text and file links.

#### Wind-down on cancel (`acp/turn.ts:261-279`)
1. Resolve `turn.cancelled`.
   - Queued asks resolve to their fallback without being sent.
   - The in-flight ask is interrupted, so the SDK sends `$/cancel_request`, and the fallback is replied to the server.
   - Fallbacks: permission `"reject"`, form `"cancel"`.
2. `POST /api/session/:id/interrupt`. Failures are logged as `"ACP server interrupt failed"`.
3. If the turn never started (no matching `session.inbox.delivered` yet), stop here.
4. Otherwise wait up to `CancelDrainTimeout` (default `"5 seconds"`, `:40-42`) for the consumer to finish successfully, which lets the server's wind-down events stream through.
5. On timeout or failure:
   - Interrupt the consumer.
   - Compute `abandon(state)`: every tracked open tool gets a `tool_call_update` failed with error `"Cancelled"` and `content: []`, plus its metadata. If the root compaction is open, it gets a cancelled compaction update (`acp/translate.ts:170-194`).
   - Send these updates, ignoring errors.

Test `test/acp/prompt.test.ts:368-402`: a cancel during prepare or before submit returns `{stopReason:"cancelled", _meta:{}}` and sends no interrupt. A cancel after submit sends exactly one interrupt.

#### Background children (`acp/turn.ts:219-224`, `:295-312`)
- When the root turn ends while child sessions are still open, a background follower is forked into the **session entry scope**, with a copy of the state and a fresh `cancelled`.
- It folds only events where `fromTrackedChild` holds: the event's sessionID, or for `session.created` its `parentID`, is a tracked child (`acp/translate.ts:132-135`).
- Background `SessionUpdate` outputs are **dropped**. `ChildUpdate` extension notifications, permission asks and form asks are still delivered.
- It ends when `openChildren` is empty, after waiting for asks to settle.
- If the session was detached, close immediately instead.

#### Event fold (`acp/translate.ts:93-130`)
State: `started`, `tools` (keyed by `${sessionID}:${toolId}`), `retries`, `compactions`, `children`, `openChildren`, `asks`, `finish`, `usage{turn,last}`, `stepError`, `executionError`.

Events are checked in this order:
1. **`session.created`** (`:200-214`):
   - If `parentID` is the root, or a tracked child, register the child `{id: data.sessionID, parentID, depth: parent? parent.depth+1 : 1, title: data.title}` and add it to `openChildren`.
   - Output child status `created` (only when the client has child updates).
   - Otherwise ignore. This applies even before `started`.
2. **`permission.asked`** for the root session or a tracked child:
   - Add `data.id` to `asks`.
   - Output `PermissionAsk{event, tool: tools[`${data.sessionID}:${data.source.id}`] if source.id, child}`.
3. **`form.created`** where `form.sessionID` is the root or a tracked child: add `form.id` to `asks` and output `FormAsk`.
4. **Settlement**:
   - The settled id is `permission.replied.data.requestID`, or `data.id` for `form.replied` and `form.cancelled`.
   - If that id is in `asks`, remove it and output `AskSettled`. The pending ask is then withdrawn: the client request is cancelled and nothing is replied.
5. Ignore events with no sessionID. The sessionID is `data.sessionID` if it is a string, or `form.created`'s `form.sessionID`. Also ignore events whose session is neither the root nor a tracked child.
6. **`session.inbox.delivered`** with `data.inboxID === start.id` sets `started = true`. Until then, every other event is ignored.
7. Route to `rootEvent` or `childEvent`.

**rootEvent** (`:216-235`):
- `session.step.started`: clear `stepError`, then run the generic handling below.
- `session.step.ended`: `recordStep(data.tokens)` and set `finish = data.finish`.
- `session.step.failed`: record `data.tokens` if present, and set `stepError = data.error`.
- `session.execution.succeeded`: terminal `succeeded`.
- `session.execution.interrupted`: terminal `interrupted`.
- `session.execution.failed`: set `executionError = data.error`, terminal `failed`.
- Otherwise: the generic handling.

`recordStep` sums `input`, `output`, `reasoning`, `cache.read` and `cache.write` into `usage.turn`, and sets `usage.last` to this step's tokens.

**childEvent** (`:237-250`). Child status output goes through the extension only:
- `session.execution.started`: status `running`.
- `succeeded`: `completed`, removed from `openChildren`.
- `interrupted`: `interrupted`, removed.
- `failed`: `failed` with `error: data.error`, removed.
- Otherwise: the generic handling for the child.

Child step token usage is **not** recorded.

**Generic handling** (`sessionEvent`, `:252-412`). Updates go through `route`. Here `sid` is the child id or the root id.

| opencode event | ACP update |
|---|---|
| `session.step.started` (only if a retry is tracked for sid) | `{sessionUpdate:"session_info_update", _meta:{"opencode/retry": null}}`, and the retry is cleared |
| `session.retry.scheduled` | `{sessionUpdate:"session_info_update", _meta:{"opencode/retry":{attempt: data.attempt, nextRetryAt: new Date(data.at).toISOString(), error: data.error}}}`, and the retry is tracked |
| `session.compaction.started/ended/failed` | see Compaction |
| `session.compaction.delta` | see Compaction (`compaction_summary_chunk`) |
| `session.text.delta` | `{sessionUpdate:"agent_message_chunk", messageId: data.assistantMessageID, content:{type:"text", text: data.delta}}` |
| `session.reasoning.delta` | `{sessionUpdate:"agent_thought_chunk", messageId: "${assistantMessageID}:reasoning:${data.ordinal}", content:{type:"text", text: data.delta}}` |
| `session.tool.input.started` | Track `{sessionID,id,name,input:{},metadata:{}}`. Emit `{sessionUpdate:"tool_call", ...pendingToolCall(id, name, input {})}` |
| `session.tool.called` | Set the tool's input to `data.input`; an untracked tool gets the name `"tool"`. Emit `{sessionUpdate:"tool_call_update", ...runningToolUpdate}` |
| `session.tool.progress` (only if tracked) | Set metadata to `data.metadata`. Emit a running update, which is the same as `called` |
| `session.tool.success` | Untrack. Emit `{sessionUpdate:"tool_call_update", ...completedToolUpdate(content=data.content, metadata=data.metadata)}` with a diff source `{toolName, input, metadata: data.metadata}` |
| `session.tool.failed` | Untrack. Emit `{sessionUpdate:"tool_call_update", ...errorToolUpdate(error=data.error.message, metadata=data.metadata ?? tracked.metadata, content=data.content ?? [])}` |

**route** (`:418-428`):
- Root updates become `SessionUpdate`.
- A child update is first projected (`acp/child.ts:39-47`):
  - `_meta` is merged with `{"opencode/child-session":{id, parentID, depth, title?}}`.
  - For `tool_call` and `tool_call_update`, `toolCallId` becomes `"${child.id}:${toolCallId}"`. If the update has a title and the child has a title, the title becomes `"${child.title}: ${title}"`.
- If the client has child updates, the projected update is sent as `ChildUpdate{type:"update", update}`. Otherwise it is sent on the parent session as a normal `session/update`, with the `_meta` and prefixes.

**Child extension notification** (`acp/child.ts:28-37`): method `opencode/session/child_update`, params:
```json
{"rootSessionId":"<root>","childSessionId":"<child.id>","parentSessionId":"<child.parentID>","depth":1,"title"?:"<child.title>",
 "type":"update","update":<SessionUpdate>}
```
or `"type":"status","status":"created"|"running"|"completed"|"failed"|"interrupted","error"?:{type,message}`.
- `title` is present only if truthy.
- Send failures are logged as `"ACP child session update failed"` and swallowed (`acp/turn.ts:141-153`).
- Diff enrichment also applies to child `update`s.

#### Response (`acp/translate.ts:150-167`, `:470-479`)
```json
{"stopReason": <reason>, "usage"?: {...}, "_meta": {} | {"opencode/retry": {attempt,nextRetryAt,error}}}
```
- `usage` is present only if any step recorded tokens. It holds the turn sums:
  - `inputTokens`, `outputTokens`, and `totalTokens = input+output+reasoning+cache.read+cache.write`.
  - `thoughtTokens` only if reasoning > 0.
  - `cachedReadTokens` only if > 0.
  - `cachedWriteTokens` only if > 0.
- `_meta` is always present. It holds `"opencode/retry"` if a root retry is still pending.
- `stopReason`, with error = `(stepError ?? executionError).type`:
  1. terminal `interrupted` or error `"aborted"`: `cancelled`
  2. `finish === "length"`: `max_tokens`
  3. `finish === "content-filter"` or error `"provider.content-filter"`: `refusal`
  4. otherwise: `end_turn`

Example in test `test/acp/prompt.test.ts:99-110`: input 100, output 40, reasoning 7, cache read 11, cache write 13 gives `totalTokens:171`.

#### Turn failure → JSON-RPC error (`acp/translate.ts:137-148`)
This applies only when the terminal is not `interrupted`. With `error = stepError ?? executionError`:
- `type === "provider.auth"` gives `AuthRequiredError`: code -32000, detail `"provider authentication required"`, data `{}`.
- Any other error type except `"aborted"` and `"provider.content-filter"` gives `ServiceFailureError{safeMessage: error.message || "OpenCode prompt failed", service:"session", errorName: error.type}`: code -32603, data `{service:"session", errorName:<type>}`.
- A step failure still fails the prompt even if `execution.succeeded` follows (test `test/acp/prompt.test.ts:334-366`).

#### Usage update (`acp/turn.ts:328-349`)
- Sent after every non-error settle, including cancelled, and before the response.
- Conditions: `used = total(usage.last)` must be greater than 0, and the current model (selection or default) must have a truthy `limit.context`.
- Then `GET /api/session/:id` for the cost, and send:
  `{sessionUpdate:"usage_update", used, size: model.limit.context, cost:{amount: session.cost, currency:"USD"}}`.
- Errors are logged as `"ACP usage update failed"` and ignored.

#### Event stream loss (`acp/turn.ts:92-97`)
- If the turn's event queue ends, the result is `ServerUnavailableError`.
- A transport or body-read failure is classified as ServerUnavailable.
- Wire error: `{code:-32603, message:"Internal error: OpenCode server is unavailable", data:{errorName:"ServerUnavailable"}}` (test `test/acp/error.test.ts:21-50`).

### Tool-call mapping (`acp/tool.ts`)
- **canonicalName** (`:181-190`): lowercase, then apply the V1 aliases `bash→shell`, `task→subagent`, `apply_patch→patch`.
- **kind** (`:16-39`):

  | Tool | Kind |
  |---|---|
  | shell | `execute` |
  | webfetch | `fetch` |
  | edit, patch, write | `edit` |
  | grep, glob, context, context7_resolve_library_id, context7_get_library_docs | `search` |
  | read | `read` |
  | subagent | `think` |
  | anything else | `other` |

- **locations** (`:41-65`). Each location is `{path}`, absolutized against cwd (`path.resolve`), deduplicated, with empty or non-string values dropped:

  | Tool | Location source |
  |---|---|
  | shell | `input.workdir ?? input.cwd ?? cwd` |
  | read, edit, write | `input.path ?? input.filePath` |
  | patch | each hunk's `path`, plus `movePath` for updates, parsed from `input.patchText` |
  | external_directory | `input.filepath` |
  | grep, glob, context, context7_* | `input.path` |
  | anything else | `[]` |

- **title** (`:160-163`):
  - shell: `input.command ?? input.cmd ?? fallback ?? toolName`.
  - Other tools: `fallback || toolName`, where fallback is the `title` argument, which is used only for pending calls from permission asks.
- **rawInput** (`:165-168`): for shell without `input.cwd` or `input.workdir`, `{...input, cwd}`. Otherwise the input unchanged.
- **pendingToolCall** (`:67-82`): `{toolCallId, name: toolName, title, kind, status:"pending", locations, rawInput}`.
- **runningToolUpdate** (`:84-98`): `{toolCallId, status:"in_progress", kind, title (no fallback), locations, rawInput}`.
- **completedToolUpdate** (`:100-125`): `{toolCallId, status:"completed", locations, content, rawOutput:{metadata?}}`. It has **no** kind, title or rawInput.
  - Content conversion (`toolContent`, `:151-158`):
    - A text part becomes `{type:"content", content:{type:"text", text}}`.
    - A file part whose `uri` matches `^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$` with an `image/*` mime becomes `{type:"content", content:{type:"image", mimeType, data}}`.
    - Other parts are dropped.
  - For a `read` tool whose first text part unwraps with `readDisplayText`, the primary content is a single text item holding the unwrapped text. Otherwise the primary content is the non-image items.
  - Images are always appended after the primary content.
  - `readDisplayText` (`packages/tui/src/mini/tool.ts:167-186`):
    - The text must start with `{` and parse as JSON.
    - If `content` is a string and `type === "text-page"` or `encoding === "utf8"`, return `content`.
    - Else, if `entries` is an array, return the entries (strings, or `entry.path` strings) joined with `"\n"`.
    - Else return undefined.
- **errorToolUpdate** (`:127-149`):
  `{toolCallId, status:"failed", kind, title, locations, rawInput, content:[...toolContent(content), {type:"content", content:{type:"text", text: error}}], rawOutput:{metadata?, error}}`.

#### Completed-edit diffs (`acp/permission.ts:83-120`)
This applies to `tool_call_update` outputs that carry a diff source: live `session.tool.success`, and replayed completed tool parts.
1. Decode `metadata.files` as `FileDiff.Info[]` (`{file, patch, additions, deletions, status: added|deleted|modified}`). If any element fails to decode, the whole list becomes none.
2. For each file, concurrently, with `path = absolutize(file.file)`:
   - `newText`: `""` if deleted, otherwise the **current on-disk content**.
   - `added`: the diff is `{type:"diff", path, oldText:null, newText}`.
   - Otherwise, parse the first patch in `file.patch` and compute `oldText = applyPatch(newText, reversePatch(recorded))` (jsdiff semantics).
     - If that succeeds, emit the diff.
     - If it fails and the tool is `patch`, find the update hunk whose `movePath ?? path` resolves to `path`. Every chunk must have non-empty old and new lines. Swap old and new lines, run `Patch.derive(hunk.path, chunks, newText)`, and accept the result only if `structuredPatch(rebuilt→newText)` hunk ranges `"oldStart,oldLines,newStart,newLines"` (space-joined) equal the recorded patch's ranges.
   - Any error skips the file.
3. Append the diffs to `content`. If there are none, the update is unchanged.

Test `test/acp/replay.test.ts:69-76` covers CRLF content being preserved.

### Permission requests (`acp/permission.ts`)
The ask is triggered by the fold output `PermissionAsk` and queued (`acp/turn.ts:154-168`).

Building the request (`:47-74`):
1. Resolve the tool:
   - `toolName = tool?.name ?? event.data.action`
   - `toolInput = tool?.input ?? event.data.metadata ?? {}`
2. **Previews** (`:123-162`). Errors give `[]`.
   - patch tool: one diff per hunk.
     - add: `{path, oldText:null, newText: contents}`, with a trailing `"\n"` appended unless the contents are empty or already end with one.
     - delete: `{path, oldText: disk, newText:""}`.
     - update: `{path: movePath ?? path (absolutized), oldText: disk, newText: Patch.derive(...).content}`.
   - Other tools: for each `FileDiff` in `event.data.metadata.files`:
     - `oldText`: `null` if added, otherwise the disk content.
     - `newText = applyPatch(oldText ?? "", file.patch)`. If the patch fails, skip the file.
3. **Title** (`:168-188`):
   - If there are more than 1 previews: `"${n} files"`.
   - external_directory: `description ?? command ?? parentDir`.
   - webfetch: `url`.
   - websearch: `query`.
   - grep, glob: `pattern`.
   - read, edit, write, patch: `path ?? filePath ?? previews[0].path`.
   - Anything else: undefined.
4. `toolCall = pendingToolCall({toolCallId: childPrefix(tool?.id ?? event.data.id), toolName, state:{input, title: title ? childPrefixTitle(title) : child?.title}, cwd})`.
5. **Locations** (`:190-200`):
   - Use `toLocations(...)`.
   - If that is empty and `event.data.action` is in {`read`,`edit`,`external_directory`}, derive them from `event.data.resources`:
     - Strip a trailing `/*`.
     - Skip empty values and values containing `*` or `?`.
     - Absolutize the rest and deduplicate.

Request params:
```json
{"sessionId":"<root client session id>",
 "toolCall":{ ...toolCall, "name": tool?.name /*omitted if no tracked tool*/, "rawInput": tool ? toolCall.rawInput : undefined,
              "locations": <permissionLocations>, "content"?: <previews if any>, "_meta"?: {"opencode/child-session":{...}} },
 "options":[
   {"optionId":"once","kind":"allow_once","name":"Allow once"},
   {"optionId":"always","kind":"allow_always","name":"Always allow"},
   {"optionId":"reject","kind":"reject_once","name":"Reject"}]}
```
Source for the options: `:39-43`.

Outcome (`:72-73`):
- `outcome.outcome === "selected"` with optionId `once` or `always` replies that optionId.
- Anything else (`reject`, `cancelled`, unknown id, client error or throw) replies `"reject"`.

Reply (`:76-81`): `POST /api/session/:event.data.sessionID/permission/:event.data.id/reply` with body `{decision}`. The sessionID may be the child's. `PermissionNotFoundError` is ignored.

Test `test/acp/permission.test.ts:25-97` gives example values:
- Shell: title `"printf hello"`, kind `execute`, locations `[{path:"/workspace"}]`, rawInput `{command, cwd:"/workspace"}`.
- Read: title is the path.

#### Asks queue semantics (`acp/turn.ts:99-132`)
- Each ask registers a `settled` deferred by id, then queues an effect. The queue is drained one ask at a time.
- Each queued effect races, in this start order:
  1. `turn.cancelled`, which yields the fallback.
  2. `settled`, which yields "settled" and means "do not reply".
  3. The ask itself.

  An earlier winner means later racers never start.
- A die or failure of the ask yields the fallback. The die case is logged as `"ACP ask failed"`.
- The reply is then sent with the outcome, unless the result was "settled".
- When a terminal is reached, `asksSettled` queues a marker and waits for it, so every earlier ask is answered before the prompt responds.

Test `test/acp/permission.test.ts:147-182`:
- A permission replied elsewhere while its ask was pending is withdrawn, and exactly one `$/cancel_request` is sent.
- A queued ask that was replied elsewhere is never sent.

### Elicitation (`acp/elicitation.ts`, `acp/turn.ts:169-186`)
Triggered by the `FormAsk` fold output.

1. **`requestedSchema(form, caps)`** (`:50-65`). It returns undefined, meaning unsupported, if any of these hold:
   - `!caps.formElicitation`.
   - `form.metadata.kind` is not `"question"` or `"websearch.provider"`.
   - Any field is credential-like: the regex `/password|passphrase|secret|token|api[_ -]?key|credential|private[_ -]?key/i` matches any of key, title, description, or an option label of a string or multiselect field.
   - Any field has `type:"external"`.
   - Any field has a non-empty `when`.
   - Any field is hidden, required, and has no default.
   - Any visible field is not representable (`:119-129`). This applies only to string fields with `options`, and to multiselect fields:
     - Every default value must be in the options.
     - If `custom` is set: not allowed if the field is required, or if the key `${key}_custom` collides with another field key. For a multiselect, also not allowed if it has `minItems` or `maxItems`.
2. If unsupported, call `cancelUnshown`. It is not queued:
   - `DELETE /api/session/:sid/form/:formID`, with query `message` set to `UnshownQuestionMessage` only when `metadata.kind === "question"`. The message is:
     `"The question couldn't be shown to the user in this client. Continue without an answer: make reasonable assumptions and state them, or ask the user in your reply if you can't proceed."` (`:35-36`)
   - Test `test/acp/turn.test.ts:139-162`.
3. **Schema** (`:60-64`, `:131-197`). The result is `{type:"object", properties, required: visible.filter(required).map(key)}`, built from the visible fields. Each property gets `base = {title, description}`, and its shape depends on the field type:
   - String without options: `{type:"string", ...base, format, minLength, maxLength, pattern, default}`. `minLength` is `max(minLength ?? 0, 1)` if the field is required.
   - String with options: `{type:"string", ...base, oneOf:[{const:value, title:label, description}], default}`.
   - Multiselect: `{type:"array", ...base, items:{anyOf:[same options]}, minItems: required ? max(minItems ?? 0, 1) : minItems, maxItems, default}`.
   - Custom option: if `custom`, add the property `${key}_custom`: `{type:"string", title:"${title ?? key} (other)", description:"Type your own answer"}`. The description is `"Add your own answer"` for a multiselect. A string field also gets its string constraints.
   - Number or integer: `{type, ...base, minimum, maximum, default}`.
   - Boolean: `{type:"boolean", ...base, default}`.
4. **Request** (`:86-97`): `elicitation/create` with
   `{mode:"form", sessionId:<root>, toolCallId?, message: childPrefixTitle(form.title), requestedSchema}`.
   - `toolCallId` is included only when the related tool call was actually streamed to the client. That requires both:
     - `toolCallSent = !background && (!child || !caps.childSessionUpdates)`
     - `form.metadata` matches `{tool:{id}}`
   - Its value is the child-prefixed `tool.id`.
5. **Answer** (`:74-84`, `:199-207`):
   - Only `action:"accept"` with content that decodes as `Form.Answer` counts. For every form field:
     - A hidden field takes its default.
     - A custom-enabled select reads `${key}_custom`. If it is a non-blank string, a string field uses it instead of the value, and a multiselect appends it (`[...value, custom]`, or `[custom]`).
     - Undefined values are omitted.
   - Anything else, including decline, cancel, a client error, or the wrong value type, gives `"cancel"`.
6. **Respond** (`:99-109`):
   - Answer: `POST /api/session/:sid/form/:formID/reply` with body `{answer}`.
   - Cancel: `DELETE /api/session/:sid/form/:formID`, with no message.
   - Errors:
     - `FormAlreadySettled` and `FormNotFound` are ignored.
     - Other reply errors are logged as `"ACP form reply failed"`, then the form is cancelled.
     - Other cancel errors fall back to `POST /api/session/:sid/interrupt`, ignoring failures (`:38-48`).
   - Test `test/acp/elicitation.test.ts:78-104`: the answer `{q0:"Bun", q1:["Fast"], q1_custom:"Small"}` is sent as `{q0:"Bun", q1:["Fast","Small"]}`.

### Compaction (`acp/compaction.ts`)
- **Standard updates** are used when `caps.compaction && (!isChild || caps.childSessionUpdates)` (`:39-44`).
- **Started**:
  - `messageId = data.inputID ?? event.id.replace(/^evt_/, "msg_")` (`:102-108`; `packages/schema/src/session-message.ts:28`).
  - The started compaction is tracked per sessionID.
- **Standard mapping** (`:110-136`):
  - Started: `{sessionUpdate:"compaction_update", compactionId, status:"in_progress"}`.
  - Ended, only if a compaction is tracked: `{sessionUpdate:"compaction_update", compactionId, status:"completed", summary: data.text ? [{type:"text", text}] : null}`.
  - Failed, with error type `"aborted"` or `"compaction.interrupted"`: `{..., status:"cancelled", summary:null}`.
  - Failed otherwise: `{..., status:"failed", summary:null, error: display}`. `display` is `"Compaction failed"` when the message is empty, or when the type is `compaction.failed` and the message contains `"\n"`. Otherwise it is `error.message`.
  - Failed with no prior started: with standard updates, emit a synthesized started update first, then the failure (`:65-74`).
  - Delta, only if tracked and standard: `{sessionUpdate:"compaction_summary_chunk", compactionId, content:{type:"text", text}}` (`:77-80`).
- **Non-standard marker**:
  - `{sessionUpdate:"session_info_update", _meta:{"opencode/compaction":{status:"started"|"completed"|"failed", messageId, reason:"auto"|"manual", error?}}}`.
  - No summary is sent, and deltas are dropped (test `test/acp/compaction.test.ts:36-52`).
- **Abandon** on cancel timeout: a failed update with `{type:"aborted", message:"Compaction cancelled"}`, which maps to `cancelled` (`:82-87`).
- **Replay**:
  - A `running` compaction message is skipped.
  - Otherwise emit the completed update with `message.summary`, or the failed update with `message.error`, using the same standard/marker choice. In replay, `isChild` is false.
- **Trigger**: a `/compact` prompt `POST`s to `/compact` with the prepared id. That id is also the turn's `start.id`, so `session.inbox.delivered{inboxID:id}` starts the turn, and `inputID` lines up with the compactionId.

### History replay on `session/load` (`acp/replay.ts`)
- **Pagination** (`:21-29`):
  - First page: `GET /api/session/:id/message?limit=200&order=asc`.
  - Next pages: `?limit=200&cursor=<next>`, until there is no `cursor.next`.
- Each update is sent as `session/update` with `{sessionId, update}`, after completed-diff enrichment (`:30-40`).
- **user message** (`:44-55`):
  - First `{sessionUpdate:"user_message_chunk", messageId, content:{type:"text", text: message.text}}`. This is always emitted, even for empty text.
  - Then one `user_message_chunk` with the same messageId per file:
    - `url = file.source.type === "uri" ? source.uri : "data:${mime};base64,${data}"`
    - `filename = file.name`, `mime = file.mime`
    - Converted with `partsToContentChunks` (below).
- **compaction message**: see Compaction.
- **assistant message** (`:60-128`). For each `content` part, in order:
  - Text: `agent_message_chunk` `{messageId: message.id, content:{type:"text", text}}`. Empty text is not filtered.
  - Reasoning: `agent_thought_chunk` with `messageId "${id}:reasoning:${index among reasoning parts}"`.
  - Tool:
    1. `{sessionUpdate:"tool_call", ...pendingToolCall(id, name, input: streaming ? {} : state.input)}`.
    2. Then by status:
       - completed: `tool_call_update` with `completedToolUpdate(metadata, content)` and diffs.
       - running: `runningToolUpdate`.
       - error: `errorToolUpdate(error: state.error.message, metadata, content)`.
       - streaming: nothing more.
- Other message types emit nothing.
- **partsToContentChunks** (`acp/content.ts:69-136`):
  - Text: skip if empty. Otherwise `{type:"text", text}`, with `annotations:{audience:["assistant"]}` if synthetic, or `["user"]` if ignored and not synthetic.
  - Reasoning text: `{type:"text", text}`.
  - `file://` url: `{type:"resource_link", uri:url, name: filename ?? "file", mimeType: mime}`.
  - A url that is not a data URL: dropped.
  - Data URL matching `^data:([^;]+);base64,(.*)$`:
    - `image/*`: `{type:"image", mimeType, data, uri: pathToFileURL(filename ?? "image").href}`. The filename is resolved against the **process cwd**, for example `file:///<cwd>/note.png`.
    - `text/*` or `application/json`: `{type:"resource", resource:{uri: pathToFileURL(filename ?? "file").href, mimeType, text: base64-decode-utf8(data)}}`.
    - Anything else: `{type:"resource", resource:{uri, mimeType, blob: data}}`.

### Prompt content conversion (`acp/content.ts:12-67`, `:138-171`)
| ACP block | Result |
|---|---|
| `text` with `annotations.audience` exactly `["assistant"]` | `{type:"text", text, synthetic:true}`. Sent via the `/synthetic` endpoint, not in the prompt text |
| `text` with audience exactly `["user"]` | `{type:"text", text, ignored:true}`. Dropped |
| `text` (other) | `{type:"text", text}` |
| `image` | `url = data ? "data:${mimeType};base64,${data}" : uri`. If there is no url, dropped. `filename = filenameFromUri(uri) ?? "image"`. A data URL becomes the file part `{url, filename, mime: mimeType}`. Otherwise `resourceLinkToPart({uri:url, name:filename, mimeType})` |
| `resource_link` | `resourceLinkToPart(block)` |
| `resource` with `text` | See "Text resources" below |
| `resource` with blob | No `mimeType`: dropped. Otherwise the file part `{url: uri starts "data:" ? uri : "data:${mimeType};base64,${blob}", filename: filenameFromUri(uri) ?? "file", mime: mimeType}` |
| other (audio, …) | dropped |

**Text resources:**
- If the uri parses as a URL with the `file:` protocol, decode it with `fileURLToPath`, falling back to `decodeURIComponent(pathname)`. The result is the text part `"[${path}${line ? ":"+line : ""}]\n${text}"`:
  - `line` comes from the hash `^#L(\d+)`.
  - On Windows, `\` becomes `/`.
- Otherwise the text part is `"[${uri}]\n${text}"`.

**resourceLinkToPart** (`:138-158`):
- A `file://` uri becomes the file part `{url: uri, filename: name || basename || "file", mime: mimeType ?? "text/plain"}`.
- A `zed://` uri with a `path` query param becomes the file part `{url: pathToFileURL(path), filename: name || basename(path) || "file", mime: mimeType ?? "text/plain"}`.
- Anything else becomes the text `name ? "[${name}](${uri})" : uri`.

**filenameFromUri** (`:164-171`):
- An empty uri or a `data:` uri gives undefined.
- A parseable URL gives the basename of the zed `path` param, or of `pathname`. An empty basename gives undefined.
- Otherwise, the basename of the raw string.

Test `test/acp/prompt.test.ts:279-322` gives an example:
- Remote https image: `[remote.png](https://…)`.
- Missing local file: `[missing.md](file://…)` inline in the text.
- Readable files and directories go into `files` with `{uri, name}`.

### Additional directories (`acp/directories.ts`)
- **parse** (`:16-24`):
  1. `parseCwd(cwd)`. A relative cwd gives `InvalidRequestError("cwd must be an absolute path: <cwd>", "cwd")`.
  2. Every entry must satisfy `path.isAbsolute` and must not contain `*` or `?`. The first invalid entry gives `InvalidAdditionalDirectoryError{directory}`: -32602, data `{additionalDirectory}`, detail `additional directory must be an absolute path without glob characters: <dir>`. An empty string is invalid.
  3. Normalize with `path.resolve` and deduplicate, preserving order.
  4. Drop entries whose `FSUtil.resolve` equals `FSUtil.resolve(cwd)`.
- **Rules** (`:71-77`):
  - For each directory, take both the literal path and `FSUtil.resolve(path)` (the realpath), deduplicated.
  - Each becomes `{action:"external_directory", resource: path.join(dir, "*"), effect:"allow"}`.
- **Metadata key**: `"opencode.acp.additionalDirectories"`, holding a string array (`:12`).
- **grant**, used on create: if there are no directories, add nothing. Otherwise add `{permissions: rules(dirs), metadata:{[key]: dirs}}`.
- **activate**, used on load, resume and fork (`:41-68`):
  - `owned = rules(previous metadata list)`.
  - New permissions: `[...rules(dirs), ...current.filter(r => !owned.some(deepEqual(r)))]`. The ACP rules come first.
  - New metadata: the existing metadata without the key, plus `[key]: dirs` if there are any directories.
  - Send `PATCH /api/session/:id` with only the changed fields, `{permissions?}` and `{metadata?}`. A field counts as changed when it differs by deep equality:
    - `permissionsChanged`: new permissions differ from current.
    - `metadataChanged`: the previous list differs from `dirs`.
  - If nothing changed, send no PATCH (test `test/acp/additional-directories.test.ts:48-81`).
- **list**: decodes the metadata key as `string[]`, or `[]` if invalid. It is used by `session/list`.

### Error mapping (`acp/error.ts:88-137`, `acp/agent.ts:47-54`, `acp/client.ts:8-24`)
The JSON-RPC `message` seen on the wire is the SDK's prefix, then `": "`, then the detail. The tests confirm two prefixes: `"Invalid params: <detail>"` and `"Internal error: <detail>"`.

| ACP error | code | data | detail (verbatim) |
|---|---|---|---|
| SessionNotFound | -32602 | `{sessionId}` | `session not found: ${sessionId}` |
| SessionDirectoryMismatch | -32602 | `{sessionId, cwd}` | `session ${sessionId} does not belong to cwd: ${cwd}` |
| InvalidConfigOption | -32602 | `{configId}` | `unknown config option: ${configId}` |
| InvalidModel | -32602 | `{providerId, modelId}` | `model not found: ${modelId}` |
| InvalidEffort | -32602 | `{effort}` | `effort not found: ${effort}` |
| InvalidMode | -32602 | `{mode}` | `mode not found: ${mode}` |
| InvalidAdditionalDirectory | -32602 | `{additionalDirectory}` | `additional directory must be an absolute path without glob characters: ${directory}` |
| AuthRequired | -32000 (`authRequired`) | `{}` | `provider authentication required` |
| UnknownAuthMethod | -32602 | `{methodId}` | `unknown auth method: ${methodId}` |
| InvalidRequest | -32602 | `{field}` if field, else `{}` | `<message>` |
| ServiceFailure | -32603 | `{service?, errorName?}` | `<safeMessage>` |
| ServerUnavailable | -32603 | `{errorName:"ServerUnavailable"}` | `OpenCode server is unavailable` |

**Classification of opencode client errors** (`acp/client.ts:8-24`):
- These become ServerUnavailable: an HTTP `TransportError`, or a `DecodeError` with a cause (a failed body read).
- Server `SessionNotFoundError` becomes `SessionNotFound{sessionId: error.sessionID}`.
- Server `InvalidRequestError` becomes `InvalidRequest{message, field}`.
- Server `InvalidCursorError` becomes `InvalidRequest{message, field:"cursor"}`.
- Anything else is a **defect**.

**Defects and unknown errors** (`acp/agent.ts:53-54`, `acp/error.ts:134-137`):
- Log `"ACP request failed"`.
- Map to `ServiceFailure{safeMessage:"Internal service failure", errorName: <Error.name if an Error>}`. On the wire: -32603, `"Internal error: Internal service failure"`. Raw details are never leaked (test `test/acp/error.test.ts:6-19`).
- An SDK `RequestError` that is already formed (for example from a client call inside a handler) passes through unchanged (`acp/agent.ts:52`).

### HTTP endpoints used (from the client calls; paths confirmed by the fake server in `test/acp/wire-fixture.ts:694-840`)

| Client call | HTTP |
|---|---|
| `event.subscribe` | `GET /api/event` (SSE; first event `server.connected`) |
| `plugin.list` / `model.list` / `model.default` / `agent.list` / `command.list` | `GET /api/plugin` / `/api/model` / `/api/model/default` / `/api/agent` / `/api/command` with `?location[directory]=` |
| `session.list` / `session.create` | `GET /api/session?directory&order&limit&cursor` / `POST /api/session` |
| `session.get` / `session.update` / `session.remove` | `GET` / `PATCH` / `DELETE /api/session/:id` |
| `session.fork` | `POST /api/session/:id/fork` |
| `session.switchModel` / `session.switchAgent` | `POST /api/session/:id/model` `{model}` / `POST /api/session/:id/agent` `{agent}` |
| `message.list` | `GET /api/session/:id/message?limit&order&cursor` |
| `session.prompt` / `command` / `compact` / `synthetic` | `POST /api/session/:id/{prompt,command,compact,synthetic}` |
| `session.interrupt` | `POST /api/session/:id/interrupt` |
| `permission.reply` | `POST /api/session/:id/permission/:requestID/reply` `{decision}` |
| `session.form.reply` / `session.form.cancel` | `POST /api/session/:id/form/:formID/reply` `{answer}` / `DELETE /api/session/:id/form/:formID?message=` |
| `mcp.add` | `PUT /api/experimental/mcp/:name?location[directory]=` `{config}` |

### Open questions
1. **SDK error message formats.** Only `"Invalid params: …"` and `"Internal error: …"` are confirmed by tests. The exact message for `RequestError.authRequired` (code -32000) and the method-not-found response for unregistered methods (for example `session/set_model`) come from the SDK and need checking against `@agentclientprotocol/sdk@1.6.0`.
2. **Extension notification method name on the wire.** Is it exactly `opencode/session/child_update`, or does the SDK prefix extension methods with `_`? The test client subscribes with the unprefixed name (`test/acp/wire-fixture.ts:445`).
3. **Default `maxMessageBytes` for `ndJsonStream`.** It is used in the "incoming message exceeded the N MiB limit" stderr text and is defined in the SDK.
4. **`$/cancel_request` handling.** Inbound cancellation of a prompt, and outbound cancellation of permission and elicitation requests, are SDK features. The exact wire shape `{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":<id>}}` is confirmed by `test/acp/permission.test.ts:138-142`. The response the SDK gives a cancelled *non-prompt* request is unverified.
5. **`pathToFileURL(filename)` in replay.** It resolves a relative filename against the agent process cwd, giving `file:///<process cwd>/<name>`. This looks like an upstream quirk. Decide whether to replicate it byte for byte.
6. **SSE subscriptions.** Each turn opens its own SSE subscription, in addition to the catalog and selection subscriptions. A Rust port could share one stream, but turn-start ordering relies on each turn's first event being `server.connected`.
7. **`SessionMessage.ID.create()` format.** The `ascending()` id generator, prefixed `msg_`, is defined in `packages/schema`. The port must generate compatible ids for `/prompt` and `/compact`.
8. **`Patch.parse`, `Patch.derive` and jsdiff behavior.** The semantics of `applyPatch`, `reversePatch`, `parsePatch` and `structuredPatch` (and their CRLF handling) affect diff output and need an equivalent Rust implementation.

---

## 15. Environment variables

Every variable read in `packages/cli`, `packages/util`, `packages/core` (non-test code). "truthy"
means `"1"` or case-insensitive `"true"` unless noted.

### 15.1 OpenCode-specific

| Variable | Read in | Meaning |
|---|---|---|
| `OPENCODE_CONFIG_DIR` | `util/src/global.ts:79`, `cli/src/index.ts:123`, `cli/src/server-process.ts:38,116` | Replaces the global config directory (`Global.config`): moves `cli.json`, `service*.json` config, global `opencode.json[c]`; also passed to the server as `config.directory` |
| `OPENCODE_CONFIG` | `server-process.ts:120` | Extra config file path for the server (see §6) |
| `OPENCODE_CONFIG_CONTENT` | `server-process.ts:121` | Inline config JSON for the server |
| `OPENCODE_CONFIG_PROJECT_DISABLE` / `OPENCODE_DISABLE_PROJECT_CONFIG` | `server-process.ts:117-119` | Truthy disables project-level config (first one set wins: `A ?? B`) |
| `OPENCODE_CLI_CONFIG_CONTENT` | `cli/src/config/config.ts:32` | Inline JSON merged over `cli.json` |
| `OPENCODE_PASSWORD` (legacy `OPENCODE_SERVER_PASSWORD`) | `cli/src/env.ts:10-13`, `server-process.ts:78-89` | Password for `--server` connections, and the password adopted by `serve` (default/stdio). Deleted from env in stdio mode. Never forwarded to the TUI session environment |
| `OPENCODE_DB` | `cli/src/database-path.ts:6`, `core/src/database/v1-migration.bun.ts` | DB filename/path (§5) |
| `OPENCODE_DISABLE_CHANNEL_DB` | `database-path.ts:8-9` | `"1"`/`"true"` (case-sensitive) → use `opencode.db` on any channel |
| `OPENCODE_PRINT_LOGS` | `framework/runtime.ts:307`, `util/src/observability/logging.ts:160`, `services/standalone.ts:29` | `"1"` → mirror logs to stderr; standalone child inherits stderr |
| `OPENCODE_LOG_LEVEL` | `logging.ts:164-173` | `DEBUG`/`INFO`/`WARN`/`ERROR` (case-insensitive); default/invalid → `INFO` |
| `OPENCODE_CLIENT` | `index.ts:132`, `default.ts:98`, `server-process.ts:97`, `acp.ts:26` | Client/app name reported to server & telemetry (default `cli`); `acp` sets it to `acp` for its child |
| `OPENCODE_TUI_CHANNEL` | `default.ts:100` | Overrides channel string passed to the TUI |
| `OPENCODE_DISABLE_AUTOUPDATE` | `cli/src/services/updater.ts:456` | `"1"`/`"true"` (case-insensitive) → skip update checks (also skipped for local builds) |
| `OPENCODE_GIT_BASH_PATH` | `server-process.ts:124`, `updater.ts` | Windows git-bash path for the server shell |
| `OPENCODE_SIMULATE` | `server-process.ts:106` | Truthy → server simulation mode |
| `OPENCODE_MODELS_URL` / `OPENCODE_MODELS_PATH` | `server-process.ts:111-112` | models.dev catalog URL / local file override |
| `OPENCODE_DISABLE_MODELS_FETCH` | `server-process.ts:113` | Truthy → don't fetch model catalog |
| `OPENCODE_FILEWATCHER_DISABLE` / `OPENCODE_DISABLE_FILEWATCHER` | `server-process.ts:127` | Truthy → disable file watcher |
| `OPENCODE_DISABLE_FFF` | `server-process.ts:128-131` | Unset → fff file finder enabled except on Windows; set → enabled iff not truthy |
| `OPENCODE_PTY_HANDOFF` | `server-process.ts:48-49` | Internal: JSON PTY handoff passed from client to a new service contender; consumed & deleted |
| `OPENCODE_SSH_ASKPASS_PORT` / `OPENCODE_SSH_ASKPASS_TOKEN` | `index.ts:23`, `ssh-askpass.ts` | Internal: run as SSH askpass helper (§0.3) |
| `OPENCODE_TEST_HOME` | `util/src/global.ts:17` | Overrides `Global.home` (tests) |
| `OPENCODE_BIN_PATH` | `bin/opencode.cjs:33` | npm shim: exact native binary to exec |
| `OPENCODE_DIRECT_TRACE`, `OPENCODE_SHOW_TTFD` | `cli/src/mini-host.ts:50,164` | `mini` diagnostics (see §12) |
| `OPENCODE_API_KEY` | `core/src/plugin/provider/opencode.ts:332` | API key for the `opencode` provider |
| `OPENCODE_REPO_CLONE_GITHUB_BASE_URL` | `core/src/repository.ts:166` | Base URL replacing `https://github.com/` for repository clones (`new URL(path + ".git", base/)`) |
| `OPENCODE_NODE_PTY_PATH`, `OPENCODE_PTY_BIN`, `OPENCODE_PARCEL_WATCHER_PATH`, `OPENCODE_PHOTON_WASM_PATH`, `OPENCODE_TREE_SITTER_WASM_PATH`, `OPENCODE_TREE_SITTER_BASH_WASM_PATH`, `OPENCODE_TREE_SITTER_POWERSHELL_WASM_PATH` | core native-asset loaders | Override paths of bundled native modules / wasm (packaging only; a Rust port can ignore or reinterpret) |

Variables **set** for children (not read): `OPENCODE_TERMINAL=1`, `TERM=xterm-256color` for PTYs
(`core/src/pty.ts:170-180`; on Windows also `LC_ALL/LC_CTYPE/LANG=C.UTF-8`); `AGENT=1`, `OPENCODE=1`,
`AI_AGENT` (default `opencode`), `OPENCODE_SESSION_ID=<id>` for shell tool commands
(`core/src/tool/plugin/shell.ts:213-216`); `OPENCODE_WORKTREE_BASE`, `OPENCODE_WORKTREE_PATH` for
worktree setup commands (`core/src/worktree.ts:244-245`); `OPENCODE_PASSWORD` for the standalone child.
`${OPENCODE_TOOL_GUIDANCE}` is a prompt-template placeholder, not an env var.

### 15.2 Standard / third-party

| Variable | Use |
|---|---|
| `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | Global path roots (§4); `XDG_CONFIG_HOME`, `SHELL`, `ZDOTDIR` also used by `uninstall` to locate shell rc files |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Enables OTLP logs (`<endpoint>/v1/logs`) and traces (`<endpoint>/v1/traces`) |
| `OTEL_EXPORTER_OTLP_HEADERS` | `k=v,k2=v2` headers for OTLP (split on `,`, first `=`) |
| `OTEL_RESOURCE_ATTRIBUTES` | `k=v,...` (URI-decoded) extra resource attributes; malformed → ignored entirely |
| `NO_COLOR`, `COLORFGBG` | `stats` color handling; `NO_COLOR=1` disables CLI-framework help colors |
| `PWD` | `run`, `mini`, `run/v1` working directory hints |
| `PATH`/`Path`, `PATHEXT`/`PathExt`, `SHELL`, `COMSPEC`/`ComSpec`, `TERM`, `TERM_PROGRAM`, `COLORTERM` | core `which`, shell selection, skill plugin |
| `SSH_ASKPASS_PROMPT` | askpass `confirm` field |
| Provider credentials: `AWS_REGION`, `AWS_DEFAULT_REGION`, `AWS_CONFIG_FILE`, `AWS_SHARED_CREDENTIALS_FILE`, `AZURE_RESOURCE_NAME`, `AZURE_COGNITIVE_SERVICES_RESOURCE_NAME`, `AZURE_CONFIG_DIR`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_GATEWAY_ID`, `GOOGLE_CLOUD_PROJECT`, `GCP_PROJECT`, `GCLOUD_PROJECT`, `GOOGLE_VERTEX_PROJECT`, `GOOGLE_CLOUD_LOCATION`, `GOOGLE_VERTEX_LOCATION`, `VERTEX_LOCATION`, `CLOUDSDK_CONFIG`, `APPDATA`, `GITLAB_TOKEN`, `GITLAB_INSTANCE_URL`, `GITLAB_OAUTH_CLIENT_ID`, `MODAL_PROXY_TOKEN`, `AICORE_SERVICE_KEY`, `AICORE_DEPLOYMENT_ID`, `AICORE_RESOURCE_GROUP`, `SNOWFLAKE_ACCOUNT`, `SNOWFLAKE_CORTEX_PAT`, `SNOWFLAKE_CORTEX_TOKEN` | Provider plugins in `core/src/plugin/provider/*` (out of scope of this document; listed for completeness) |
| Any integration env method names | `core/src/integration.ts:407,725`: an integration is "connected via environment" when one of its declared env var names is set |
| `${VAR}` in provider `baseURL` | `core/src/model-resolver.ts:290`: replaced by `process.env[VAR]`, left verbatim if unset |
| `{env:VAR}` | config / well-known substitution (see §6) |

---

## 16. Logging and diagnostics

### 16.1 Log destinations (`util/src/observability.ts`, `util/src/observability/logging.ts`)

- **File**: `<data>/log/opencode.log` for every non-`local` channel; `<data>/log/opencode-local.log`
  for `local` builds (`file(local, channel)`: `local ? "opencode-" + sanitize(channel) + ".log" : "opencode.log"`,
  called with `local = channel === "local"`). Opened in append mode; shared by **all** opencode processes
  of that channel family (CLI, background service, standalone servers).
  Writes are batched (1 s window).
- **stderr**: only while `OPENCODE_PRINT_LOGS === "1"` (checked per record); same line format.
- **OTLP**: only if `OTEL_EXPORTER_OTLP_ENDPOINT` set; resource `service.name=opencode`,
  `service.version`, attributes `deployment.environment.name=<channel>`, `opencode.client=<client>`,
  `opencode.run=<runID>`, `service.instance.id=<runID>`. If building the OTLP layer fails, falls back
  to file+stderr only.
- **Minimum level**: `OPENCODE_LOG_LEVEL` (§15), default `Info`; the framework's `--log-level` flag
  overrides it for the handler.

### 16.2 Line format (logfmt-like)

```
timestamp=<ISO-8601 UTC ms> level=<DEBUG|INFO|WARN|ERROR|FATAL|TRACE> run=<runID> <message fields> [cause=<pretty>] <span fields> <annotation fields>
```

- `runID` = first 8 chars of a random UUID, fixed per process.
- Message parts: each logged message value; a plain object (prototype `Object` or null) is flattened
  into `key=value` pairs with dotted paths for nested plain objects (`a.b=1`); empty nested object
  → `prefix={}`; circular → `[Circular]`; non-plain values become `message=<value>`.
  E.g. `Effect.logInfo("cli starting", {version, ...})` →
  `message="cli starting" version=2.0.26 channel=latest local=false args=["run","hi"]`.
- `cause=` present only when the record has a failure cause (Effect pretty-printed cause).
- Spans: `<label>=<elapsedMs>`; annotations flattened like messages (e.g. `role=cli`, `role=server`).
- Value encoding: strings as-is, other values via Effect `Formatter.format` (arrays `[a,b]` with JSON
  strings inside, numbers/booleans plain, Dates ISO, objects `{k:v}`, errors via `toString()` plus
  ` (cause: …)`); the resulting text is emitted bare if it matches `^[^\s="\\]+$`, otherwise
  `JSON.stringify`-quoted.
- Records joined with `\n`; each batch ends with `\n`.

### 16.3 Size bounding (no rotation)

`trim(target)` runs at logger start and then every hour in each process (`logging.ts:250-305`):
- Lock: `mkdir <log>.trim` (a directory); if it exists and its mtime is older than 5 min, it is removed
  (but this round is still skipped); otherwise skip.
- If file size ≤ 50 MiB (`50*1024*1024`) do nothing. Else find the first byte after the first `\n`
  at or after offset `size - 25 MiB`, copy from there to EOF (64 KiB chunks, reading to current EOF) to
  the start of the file in place, then truncate to the bytes written. Remove the lock directory.

### 16.4 Signals → diagnostics (POSIX only)

- `SIGUSR1`: write V8 heap snapshot to `<log>/heap-<pid>-<ISO timestamp without ':' and '.'>.heapsnapshot`
  (logs `writing heap snapshot` / `heap snapshot written` / `failed to write heap snapshot`).
- `SIGPROF`: capture a 10 s CPU profile to `<log>/cpu-<pid>-<timestamp>.cpuprofile`.
These are JS-runtime specific; a Rust port may map them to equivalent profiling or omit them.

### 16.5 Notable log messages (for parity of diagnostics)

`cli starting`, `cli process failed`, `uncaught exception`, `unhandled rejection`,
`background service starting`, `managed service registration check failed; shutting down`,
`managed service registration replaced; shutting down`,
`Background service is unresponsive; recovery cannot preserve persistent terminals`,
`Background service is not ready; replacement cannot preserve persistent terminals`,
`Failed to prepare persistent terminals for replacement`, `background service boot failed`,
`failed to migrate cli config`, `failed to persist migrated cli config`, `update check skipped`.

---

## 17. Open questions / uncertainties (cross-cutting)

1. **Effect CLI exact rendering.** Help text/column layout and error formatting are produced by Effect
   `effect/cli` 4.0.1; the details above were verified against `effect@4.0.0-beta.83` source, which may
   differ slightly (e.g. whether `--log-level`/`--completions` are listed in help, boolean `--no-` negation
   for every boolean flag, KeyValuePair syntax `--param a=1` vs `--param a=1 b=2`).
2. **`Cause.pretty` format** for uncaught errors written to stderr (message plus JS stack trace). A port
   should at least print `Error: <message>`; byte parity with JS stacks is impossible.
3. **IPv6 URL formatting** of `server listening on …` / registration `url`: Effect 4.0.1 formats TCP
   addresses as `http://<hostname>:<port>`; whether IPv6 hosts are bracketed there was not verified
   (`serviceURL()` in `server-process.ts:206-208` does bracket).
4. `toLocaleString()` output in `session list`/`session export` depends on host locale/ICU; exact parity
   requires emulating the JS default locale formatting.
5. The TUI (`@opencode/tui`) and the HTTP server (`@opencode/server`, `@opencode/core`) are out of scope;
   only their CLI-facing contracts are specified here.
6. Windows behaviors (retained image links, `cmd /c more` pager, process termination semantics of
   `SIGTERM`/`SIGKILL` via `process.kill`) were read from source but not exercised.
7. The section-level open-question lists in §6, §13 and §14
   list further item-specific uncertainties.

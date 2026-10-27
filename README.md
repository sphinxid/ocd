# ocd

ocd is a Rust reimplementation of [opencode](https://github.com/anomalyco/opencode) v2, the terminal AI coding
agent. It aims to behave exactly like upstream: same CLI, same TUI, same HTTP API, same config files and data
folders. It's just faster and lighter.

Only the program is renamed. The binary is `ocd` and the TUI logo reads `opencode-d`. Everything else stays
opencode-compatible: `opencode.json`, `.opencode/`, `~/.config/opencode`, `~/.local/share/opencode`, `OPENCODE_*`
environment variables, the HTTP API, plugin packages and the background service.

Upstream reference: `anomalyco/opencode`, tag `v2.1.4` (commit `68bc26b`). ocd was first ported from branch `v2` at
commit `4617210` (version 2.0.26) and has since caught up with 2.1.4 ([docs/UPGRADE-2.1.4.md](docs/UPGRADE-2.1.4.md)).

> **Shared data warning.** Because ocd uses the same data folder as opencode, running `ocd` uses your real
> opencode sessions and credentials and applies the v2 database migrations to them. To try it without touching
> your opencode data, use the sandbox launcher (see [Try it safely](#try-it-safely)).

## Speed and size

Measured on Linux x86_64, release builds of both, same mock model, isolated folders:

| Measure | opencode v2 | ocd |
|---|---|---|
| `run` with one tool call | 1.52 s | 0.79 s |
| Server ready after start | 373 ms | 139 ms |
| Idle background server memory | 151 MB | 26 MB |
| Peak memory during `run` | 293 MB | 247 MB |
| Binary size (web UI included) | 210 MB | 96 MB |

## Status: what works and what doesn't

This status is against opencode 2.1.4, and ocd reports opencode version 2.1.4 wherever it must look like
opencode. It caught up from 2.0.26 in the batches listed in [docs/UPGRADE-2.1.4.md](docs/UPGRADE-2.1.4.md): data,
providers, sessions and tools, platform and config, the HTTP API, the TUI (with the `/connect` integration dialog),
the release (version, CLI help, web UI, runtime pins) and `pair --remote` are done; of the experimental 2.1
features (batch 8) only the todo list is. Its progress table lists what each batch checked.

How the comparison is done: both binaries run the same 72 scenarios side by side and their outputs are diffed
(`scripts/parity.py`, see [docs/PARITY.md](docs/PARITY.md)). These cover the agent loop, sessions, permissions,
forms, read-only endpoints, 74 error cases, the CLI, plugins, GitHub Copilot, the OpenAI WebSocket transport and
the experimental todo list.
Every request ocd sends to the model (system prompt, tool definitions, headers, prompt-cache markers) is compared
too. Against the 2.1.4 build, 65 of the 72 scenarios are identical. The others differ by timing races that also
vary between two runs of upstream, the intended renames, the wording of a network
error, and how `ocd run` prints the last step of a run (live, where upstream prints it afterwards); PARITY.md has
the details. Each crate also has its own test suite, many replaying recorded upstream data.

### Works the same as upstream

- **Sessions and history:** create, fork, revert, export, import, search, and the same database file and schema.
- **The agent loop:** steps, tool calls, reasoning, retries, compaction, titles, background jobs and subagents.
- **Permissions and forms:**
  - permissions: ask, once, always, and reject with feedback;
  - forms: the question tool.
- **Built-in tools:** read, write, edit, patch, glob, grep, shell, webfetch, websearch, todo, task and others.
  Output limits and scanners were checked against 81,787 upstream cases.
- **Code Mode:** the JavaScript `execute` tool has its own interpreter. 1,704 of 1,707 recorded upstream programs
  replay identically; one of the others changed on purpose in 2.1 and is checked against 2.1.4 instead.
- **Project setup:**
  - agents, commands, skills (including remote skill discovery), `AGENTS.md` instructions and references;
  - config files and live reload, configured `instructions` (files, globs and URLs), `policies` and managed
    system config (`/etc/opencode` and its platform equivalents).
- **Import of opencode v1 history** into the v2 tables. The result is byte-identical to upstream, and the TUI
  shows the same progress badge.
- **Chat providers:** Anthropic, OpenAI, Google, Bedrock, Mistral, Cohere, OpenAI-compatible, OpenCode Zen and
  every models.dev provider that uses those protocols. 229 recorded upstream streams decode identically.
- **Sign-in:** API keys, OAuth, ChatGPT (token sharing and the Codex browser and device-code logins, with
  upstream's model list, endpoints and request headers for each), GitHub Copilot and the Azure CLI, from
  `ocd auth` or the TUI's `/connect` (accounts, sign-in forms, browser and device OAuth, pasted codes and login
  commands).
- **GitHub Copilot models:** the account's model list, routing between the AI SDK, Responses, Chat and native
  Anthropic routes, and Copilot's request headers. Upstream's in-house Copilot AI SDK provider runs in the Bun helper.
- **OpenAI Responses over WebSocket** for OpenAI, Azure and xAI: one socket per session, incremental turns with
  `previous_response_id`, and the same fallback to HTTP. Proxies and WebSocket compression are not supported yet.
- **MCP servers:** stdio and HTTP transports, OAuth and elicitation.
- **Server plugins:** tools, hooks, transforms, storage, rpc and events, plus `ocd plugin add/check/update`. They
  run in a small Bun helper process, as listed below.
- **AI SDK provider packages** such as `@ai-sdk/deepseek`, through the same Bun helper.
- **Clients and API:**
  - the full TUI;
  - the web UI served by `ocd serve`, built from upstream's v2.1.4 app (88 of 88 compared requests were
    identical with the 2.0.26 app; upstream's serving code is unchanged in 2.1.4);
  - the HTTP API (149 endpoints, from upstream 2.1.4's OpenAPI document) and the event stream;
  - the background service (`ocd service start/stop/status/restart`) on port 49374, or 28894 on Windows,
    like upstream 2.1.
- **Git:** status, diffs, branches, snapshots and worktrees.

### Works, with a small difference

| Area | Difference |
|---|---|
| Plugins and AI SDK packages | Upstream runs them inside its own Bun-based binary. ocd runs them in a Bun helper process and downloads Bun 1.4.4 the first time a plugin or AI SDK package is used (or set `OCD_BUN_PATH`). |
| Built-in AI SDK packages | Upstream bundles `@ai-sdk/cohere`, `@ai-sdk/perplexity`, `@ai-sdk/vercel` and `gitlab-ai-provider`. ocd installs them from npm on first use, at the same versions. If an install fails, the error wording differs. |
| `session.http.response` plugin hook | The hook receives the whole response body as text instead of a stream. |
| Plugin worktree strategies | Worktree tools use plugin strategies only after the project's location has loaded. They otherwise fall back to git. |
| Persistent terminals | Same `opencode-pty` helper (0.3.1). ocd downloads it once instead of carrying it inside the binary. On Windows it is enabled like upstream 2.1 but not yet tested there. |
| Mini mode (`ocd mini`) | Works, but is simplified and not compared screen by screen. |
| `ocd auth login` pickers | Multi-select asks for comma-separated values. |
| Editor integration (ACP) | Works, but doesn't add the diff for a completed edit. |
| Error texts | A few come from Rust instead of Bun or Node: file-system errors, MCP JSON-schema validation and network errors. |
| Start-up events | A new session's start-up notifications arrive about 0.5 s sooner. They are the same events in the same order. |
| Remote access (`pair --remote`) | A native OpenTunnel client (`crates/oc-tunnel`) replaces upstream's JavaScript SDK. It keeps the tunnel in the same place, so ocd, upstream and the opentunnel CLI share a device's tunnel. Tested against a local stand-in for the service, not yet against opentunnel.xyz. |

### Not implemented yet

| Feature | Notes |
|---|---|
| TUI plugins (the `tui` entrypoint) | Not planned: they need upstream's SolidJS terminal UI. |
| Third-party packages for upstream's own model runtime (`@opencode/ai`) | AI SDK packages do work. |
| Image, speech, video and transcription models | Upstream includes these only as library code that nothing in the app calls. |
| Mercurial repositories | Has no version-control provider, so its endpoints answer empty. |
| `--wizard` mode | Prints an error. |
| Experimental voice and client-hosted MCP endpoints | Listed in the API document like upstream's, but answer 500 (not implemented). |
| Other experimental 2.1 features | Background compaction, `location_idle_timeout` and the new designs are not ported; their config keys are accepted and do nothing. The todo list (`experimental.todo`: the `todowrite` tool, the sidebar section and its transcript rendering) is ported. |
| Some cloud credentials | AWS SSO, web identity and instance metadata, and Google workload identity federation, are missing. Env vars, shared config files, `credential_process` and ECS credentials work. |
| Testing on macOS, Windows and Linux ARM | Release binaries are built for them, but only Linux x86_64 is tested. |

### Different on purpose

- **Working status line in the TUI:** after you press Enter, a line above the prompt shows what is happening
  (waiting for the model, thinking, writing, running a tool, retrying, compacting), an elapsed timer that ticks every
  second, the output token count and the interrupt key. A running shell command goes on the row below it. Upstream
  only shows its moving progress bar, which ocd keeps, and a live "Thinking" row in the transcript, which ocd leaves
  out while the status line shows it.
- **Name:** the program name, help text and logo say `ocd` / `opencode-d`.
- **`ocd upgrade`:** prints how to rebuild from source instead of downloading a release.
- **Heap snapshots:** ocd's server has no JavaScript heap, so `POST /api/debug/heap-dump` answers `500 UnknownError`.
  `ocd debug heap-dump --server URL` still captures one from an upstream server.
- **Version and release channel:** `ocd --version` shows ocd's own build (`<tag>+<commit>`, or the build time and
  commit without a tag) next to the opencode version it is compatible with (2.1.4); clients, plugins and the service
  registration see 2.1.4 on the `latest` channel.

## Releases

Pushing a tag that starts with `v` builds ocd for every platform and publishes the binaries on the GitHub
release for that tag (`.github/workflows/release.yml`):

```sh
git tag v1.0.0
git push origin v1.0.0
```

| File | Platform |
|---|---|
| `ocd-<tag>-linux-x64.tar.gz` | Linux x86_64, glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later) |
| `ocd-<tag>-linux-arm64.tar.gz` | Linux ARM64, glibc 2.35 or newer |
| `ocd-<tag>-windows-x64.zip` | Windows x86_64 |
| `ocd-<tag>-darwin-arm64.tar.gz` | macOS on Apple silicon |

`SHA256SUMS` lists the checksums. The binaries include the web UI. `ocd --version` shows the tag and commit,
for example `ocd v1.0.0+1a2b3c4 (opencode 2.1.4)`; the opencode version is the one ocd reports to providers,
plugins and the service, so it stays compatible. To rebuild an existing tag, run the workflow from the Actions
tab with the tag name.

The macOS binary isn't signed. After unpacking, clear the download quarantine once:
`xattr -d com.apple.quarantine ocd`.

## Building

### Requirements

- Linux x86_64, the only tested platform.
- Rust via [rustup](https://rustup.rs). Edition 2024 needs at least Rust 1.85; development uses 1.99.
- A C compiler (`gcc` or `clang`), needed for the bundled SQLite and `ring`.
- About 15 GB of free disk for a full debug and test build. A release build needs less.

### Build the binary

```sh
git clone https://github.com/sphinxid/ocd.git
cd ocd
cargo build --release -p ocd
./target/release/ocd --version     # ocd v0.2.0+e80344b (opencode 2.1.4)
```

The release build is optimised and stripped. The binary is self-contained: it holds the plugin helper and,
optionally, the web UI. It links only against the system C library, so another Linux x86_64 machine needs
glibc of the same version or newer. A binary built on Ubuntu 24.04 needs glibc 2.39, which means Ubuntu 24.04,
Debian 13, Fedora 40 or newer.

`ocd --version` shows the ocd version next to the opencode version it is compatible with. The build works it
out from the git tags the same way the release workflow does: a checkout of tag `v0.2.0` reports
`0.2.0+<commit>`, exactly like the release binary of that tag, and three commits later it reports
`0.2.0+3.<commit>`. Run `git fetch --tags` first so the latest tag is there. Without a `v*` tag the version is
the build time in UTC and the commit (`20261027.124153+<commit>`), and outside a git checkout a random hash
takes the commit's place. To set the version yourself, build with `OCD_BUILD_VERSION`,
for example `OCD_BUILD_VERSION=1.0.0+$(git rev-parse --short HEAD) cargo build --release -p ocd`.

### Optional: include the web UI

The web UI is upstream's web app, about 24 MB, so it isn't committed. Without it, `ocd serve` answers 404 for
web pages, the same as an upstream build made with `--skip-web-ui`. The TUI and CLI don't need it.

To include it, build it from an upstream checkout at `v2.1.4` with Bun 1.4.4, then rebuild ocd. If `node` is on your PATH it
must be Node 22.12 or newer (or 20.19+), because the app's bundler runs under it. The archive is embedded at
compile time:

```sh
git clone https://github.com/anomalyco/opencode.git ../opencode
(cd ../opencode && git checkout v2.1.4 && bun install)
BUN=bun scripts/build-web-ui.sh ../opencode     # writes crates/ocd/web-ui/app-archive.bin
cargo build --release -p ocd
```

At runtime ocd looks for the web UI in this order:

1. the `OCD_WEB_UI` environment variable, pointing to an archive file or an unpacked `dist` folder;
2. the archive embedded in the binary;
3. an `app-archive.bin` file next to the binary.

### Optional: rebuild the plugin helper

`crates/oc-plugin/host/dist/host.js` is committed, so building ocd never needs Bun. If you change the helper's
TypeScript (`crates/oc-plugin/host/src/`), rebuild the bundle from an upstream checkout at `v2.1.4` (with
`bun install` run) using Bun 1.4.4:

```sh
BUN=/path/to/bun-1.4.4 scripts/build-plugin-host.sh ../opencode
```

## Running

```sh
ocd                      # TUI; starts the background server if it isn't running
ocd --standalone         # TUI with a private server that exits with it
ocd run "explain this repo" -m provider/model
ocd serve                # HTTP API and web UI in the foreground
ocd auth login           # sign in to a provider
ocd service status       # the background server
ocd service stop
ocd --help
```

**The background server keeps running after you quit the TUI,** as upstream's does. Later `ocd` runs and web UI
tabs connect to it instantly. Stop it with `ocd service stop`. To never start it, run
`ocd service set disabled true`, after which every `ocd` behaves like `--standalone`.

**`ocd run` reads extra prompt text from stdin** when stdin isn't a terminal, as upstream does. In scripts, pass
`</dev/null` if you have nothing to pipe in.

### Try it safely

`scripts/ocd-sandbox` runs `target/release/ocd` with its own config, data, state and cache folders under
`~/.local/share/ocd-sandbox`. It never touches your opencode data:

```sh
scripts/ocd-sandbox auth login
scripts/ocd-sandbox --standalone
```

### Environment variables specific to ocd

| Variable | Purpose |
|---|---|
| `OCD_BUN_PATH` (or `OPENCODE_BUN_PATH`) | Use this Bun binary for plugins and AI SDK packages instead of downloading one |
| `OCD_BUN_MIRROR` | Download Bun from this mirror |
| `OCD_PLUGIN_HOST_JS` | Use this plugin helper bundle instead of the embedded one |
| `OCD_WEB_UI` | Serve the web UI from this archive or folder |

All upstream `OPENCODE_*` variables work as they do upstream.

### Certificates

ocd trusts the same certificates as upstream's runtime (Bun 1.4.4, built with `--use-system-ca`), for HTTPS and
for the WebSocket transport:

- the Mozilla root list;
- the system store: every certificate in the usual bundle files, such as `/etc/ssl/certs/ca-certificates.crt`,
  and in the usual certificate folders, such as `/etc/ssl/certs`;
- the PEM certificates in the file named by `NODE_EXTRA_CA_CERTS`.

`SSL_CERT_FILE` replaces the bundle files and `SSL_CERT_DIR` (a `:`-separated list) replaces the folders.
`NODE_TLS_REJECT_UNAUTHORIZED=0` turns certificate checks off.

If the `NODE_EXTRA_CA_CERTS` file can't be read or holds no certificate, the server prints
`warn: ignoring extra certs from <file>, load failed: <reason>` on its error output and goes on without it, as
upstream does. ocd also writes the line to its log.

So behind a TLS-inspecting proxy, or with a provider on a private certificate authority, set
`NODE_EXTRA_CA_CERTS` to the authority's certificate file.

### When something stops or crashes

- **A run ended before its tasks were done:** `sh scripts/why-stopped.sh ses_…` shows, read-only, how the
  session's last runs ended (interrupted, model finished, or failed with a message).
- **A process crashed:** every panic in any ocd process is logged as `panicked` with its message and source
  line. Find it with `grep panicked ~/.local/share/opencode/log/opencode.log`.
- **The background service stopped:** an open TUI starts it again by itself, like upstream, and `/restart`
  restarts it by hand. A stop by signal is logged as `server stopping signal=SIGTERM` (or SIGHUP, SIGINT); if the
  service vanished without that line it was killed with SIGKILL. On systems where logging out kills your
  processes (`loginctl show-user "$USER" -p KillUserProcesses` says `yes`), run `loginctl enable-linger "$USER"`
  so the service survives an SSH logout.
- **The terminal is garbled after the TUI died:** on a crash or on SIGTERM, SIGHUP, SIGINT, SIGQUIT or SIGABRT,
  the TUI turns raw mode and mouse reporting off before it exits, like upstream. SIGKILL (for example from the
  out-of-memory killer) can't be caught; type `reset` and press Enter to fix the terminal.

### Downloads on first use

When needed, ocd downloads these tools into the opencode cache folder, the same way upstream does:

- `ripgrep`, used by glob and grep when `rg` isn't installed;
- Bun 1.4.4, only for plugins or AI SDK packages;
- the `opencode-pty` helper, for persistent terminals;
- npm packages for plugins and AI SDK providers.

## Testing

```sh
cargo test --workspace --no-fail-fast
```

Some tests need the upstream checkout, the upstream binary or Bun 1.4.4. They read these from a local working
folder named by `OCD_SCRATCH`, and are skipped when it's unset:

```
$OCD_SCRATCH/src/opencode                    upstream checkout (tag v2.1.4, `bun install` done)
$OCD_SCRATCH/up-dist/cli-linux-x64/bin/opencode   upstream binary built from that checkout
$OCD_SCRATCH/bun144/bun-linux-x64/bun        bun 1.4.4
```

```sh
export OCD_SCRATCH=$HOME/ocd-scratch
export OCD_BUN_PATH=$OCD_SCRATCH/bun144/bun-linux-x64/bun
cargo test --workspace --no-fail-fast
python3 -B scripts/parity.py -q              # side-by-side comparison with upstream (~25 min)
```

Run tests and parity runs with isolated `XDG_*_HOME` folders and `OPENCODE_TEST_HOME`, so they never touch your
real opencode data. `docs/PORTING.md` has the details.

## Repository layout

| Path | What it holds |
|---|---|
| `crates/ocd` | The `ocd` binary: CLI commands, serve/service modes, web UI embedding |
| `crates/oc-core` | Sessions, the agent loop, permissions, agents, skills, MCP, plugins, worktrees, v1 import |
| `crates/oc-backend` | Implements the HTTP API on top of oc-core |
| `crates/oc-server` | HTTP/SSE server, auth, CORS, web UI serving, generated API types |
| `crates/oc-tui` | Terminal UI (ratatui), full and mini modes |
| `crates/oc-ai` | Model catalogue (models.dev), provider protocols, credentials and OAuth, AI SDK route |
| `crates/oc-tools` | Built-in tools, ripgrep, websearch, output truncation |
| `crates/oc-codemode` | JavaScript interpreter for the Code Mode `execute` tool |
| `crates/oc-plugin` | Bun plugin helper (`host/`) and the Rust side of its protocol |
| `crates/oc-mcp` | MCP client: stdio, streamable HTTP, OAuth |
| `crates/oc-config` | Config schema, discovery, normalisation, policies |
| `crates/oc-storage` | SQLite database, migrations, event projectors |
| `crates/oc-schema` | IDs, messages, session events |
| `crates/oc-git` | Git, snapshots, worktree strategy, VCS |
| `crates/oc-persistent-pty` | Client for the `opencode-pty` daemon |
| `crates/oc-util` | Paths, logging, JSONC, file locks, tracing headers |
| `assets/` | Verbatim upstream assets: prompts, themes, models.dev snapshot, OpenAPI document, SQL migrations |
| `docs/` | Specifications (`docs/spec/`), parity results (`PARITY.md`), porting rules (`PORTING.md`), status (`STATE.md`) |
| `scripts/` | Parity suite, mock provider, web UI and plugin helper build scripts, sandbox launcher |

Each crate has its own `README.md` with its API and any known differences from upstream.

## License

ocd is MIT-licensed, see [LICENSE](LICENSE). It is a derivative of opencode, which is also MIT-licensed
(copyright opencode), see [LICENSE-opencode](LICENSE-opencode).

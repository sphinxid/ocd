# ocd

**ocd is [opencode](https://github.com/anomalyco/opencode) v2, the terminal AI coding agent, rewritten in Rust.**
It has the same CLI, TUI, HTTP API, config files and data folders, and it starts faster and uses far less memory.

- **Drop-in:** it reads your `opencode.json`, `.opencode/`, `~/.config/opencode`, sessions, credentials, plugins
  and `OPENCODE_*` variables. Only the program is renamed: the binary is `ocd` and the logo reads `opencode-d`.
- **Up to date:** it tracks opencode **2.1.4** (tag `v2.1.4`, commit `68bc26b`) and is compared against it side by
  side ([how](docs/COMPATIBILITY.md#how-it-is-compared)).
- **One self-contained binary:** TUI, server and web UI. It upgrades itself from its own releases with `ocd upgrade`.

> [!WARNING]
> ocd uses the **same data folder as opencode**: your real sessions and credentials, with the v2 database
> migrations applied. To try it without touching them, use the [sandbox](docs/USAGE.md#try-it-safely).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash
```

No curl? Use `wget -qO- https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash`.

The script installs the latest release for your system as `~/.local/bin/ocd` (no root needed), checks it against
the release checksums and adds `~/.local/bin` to your `PATH` if it isn't there. Run it again to update; if ocd was
already installed, it also restarts the background service.

| Platform | Supported |
|---|---|
| Linux x86_64 and ARM64 | glibc 2.17 or newer: RHEL/CentOS 7+, Ubuntu 14.04+, Debian 8+ (not Alpine/musl) |
| macOS | Apple silicon |
| Windows x86_64 | Download the `.zip` from [Releases](https://github.com/sphinxid/ocd/releases) |

Installer options, manual installs and upgrading: **[docs/INSTALL.md](docs/INSTALL.md)**.

## Quick start

```sh
ocd auth login                            # sign in to a provider (or use /connect in the TUI)
ocd                                       # open the TUI in the current project
ocd run "explain this repo" -m provider/model
ocd serve                                 # HTTP API and web UI in the foreground
ocd upgrade                               # install the latest release
ocd --help
```

The TUI starts a background server that keeps running after you quit, like upstream's, so later runs connect
instantly. Manage it with `ocd service status|stop|restart`, or run `ocd --standalone` to skip it. More in
**[docs/USAGE.md](docs/USAGE.md)**.

## Speed and size

Linux x86_64, release builds of both, the same mock model and isolated folders:

| Measure | opencode v2 | ocd |
|---|---:|---:|
| `run` with one tool call | 1.52 s | **0.79 s** |
| Server ready after start | 373 ms | **139 ms** |
| Idle background server memory | 151 MB | **26 MB** |
| Peak memory during `run` | 293 MB | **247 MB** |
| Binary size (web UI included) | 210 MB | **96 MB** |

## Compatibility at a glance

| Status | Areas |
|---|---|
| ✅ **Same as upstream** | Sessions and history, the agent loop, permissions, built-in tools, Code Mode, agents/commands/skills, config and policies, all chat providers and sign-in methods (API keys, OAuth, ChatGPT, GitHub Copilot, Azure, AWS and Google cloud credentials), MCP servers, server plugins and AI SDK packages, the full TUI, web UI, HTTP API (149 endpoints), background service, git and worktrees, v1 history import |
| 🟡 **Small differences** | Plugins run in a Bun helper that is downloaded on first use; a few error texts; mini mode and the ACP editor integration are simplified |
| ⛔ **Not yet** | TUI plugins, image/speech/video models, Mercurial, `--wizard`, most experimental 2.1 features (the todo list is ported) |
| ✨ **Different on purpose** | A working status line in the TUI, `ocd upgrade` from ocd's own releases, `ocd --version` shows ocd's own version |

65 of 72 side-by-side scenarios produce identical output; the rest differ only by timing, the renames and a few
message wordings. Details: **[docs/COMPATIBILITY.md](docs/COMPATIBILITY.md)**.

Only Linux x86_64 is tested day to day. The macOS, Windows and ARM64 builds are produced by CI but not tested.

## Build from source

```sh
git clone https://github.com/sphinxid/ocd.git && cd ocd
cargo build --release -p ocd          # needs Rust 1.85+ and a C compiler
./target/release/ocd --version        # ocd v0.2.2+1a2b3c4 (opencode 2.1.4)
```

Web UI, version stamping, tests and releases: **[docs/BUILDING.md](docs/BUILDING.md)**.

## Documentation

| Guide | What's in it |
|---|---|
| [Install and upgrade](docs/INSTALL.md) | Installer options, release files, manual install, `ocd upgrade` and update checks |
| [Using ocd](docs/USAGE.md) | Commands, the background service, sandbox, environment variables, certificates, troubleshooting |
| [Compatibility](docs/COMPATIBILITY.md) | What matches upstream, what differs and what's missing, and how it's compared |
| [Building and contributing](docs/BUILDING.md) | Building, web UI, testing, releases, repository layout |
| [Parity results](docs/PARITY.md) | The side-by-side comparison with upstream, scenario by scenario |
| [Porting rules](docs/PORTING.md) and [state](docs/STATE.md) | How the port is done, and where it stands |

## License

MIT, see [LICENSE](LICENSE). ocd is a derivative of opencode, which is also MIT-licensed (copyright opencode), see
[LICENSE-opencode](LICENSE-opencode).

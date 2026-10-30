# Building and contributing

[← README](../README.md)

- [Requirements](#requirements)
- [Build the binary](#build-the-binary)
- [Version numbers](#version-numbers)
- [Include the web UI](#include-the-web-ui)
- [Rebuild the plugin helper](#rebuild-the-plugin-helper)
- [Testing](#testing)
- [Releases](#releases)
- [Repository layout](#repository-layout)

Before changing code, read [PORTING.md](PORTING.md) (the porting rules) and [STATE.md](STATE.md) (where the port
stands).

## Requirements

| Need | Notes |
|---|---|
| Linux x86_64 | The only tested platform |
| Rust via [rustup](https://rustup.rs) | Edition 2024 needs Rust 1.85 or newer; development and releases use 1.99 |
| A C compiler (`gcc` or `clang`) | For the bundled SQLite, zstd and `ring` |
| Disk space | About 15 GB for a full debug and test build; a release build needs less |

## Build the binary

```sh
git clone https://github.com/sphinxid/ocd.git
cd ocd
cargo build --release -p ocd
./target/release/ocd --version
```

The release build is optimised and stripped. The binary is self-contained: it carries the plugin helper and,
optionally, the web UI.

It links only against the system C library, so it runs on machines with the same glibc version or newer. A
binary built on Ubuntu 24.04 needs glibc 2.39, which means Ubuntu 24.04, Debian 13, Fedora 40 or newer. For older
systems, use the release's glibc 2.17 builds, or build the same way the release does:
`cargo zigbuild --release -p ocd --target x86_64-unknown-linux-gnu.2.17` with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild).

## Version numbers

The build works out its version from the git tags, the same way the release workflow does:

| Checkout | `ocd --version` |
|---|---|
| Exactly at tag `v0.2.2` | `ocd v0.2.2+<commit> (opencode 2.2.2)`, the same as that release's binary |
| Three commits after `v0.2.2` | `ocd v0.2.2+3.<commit> (opencode 2.2.2)` |
| No `v*` tag | `ocd v20261027.124153+<commit> (opencode 2.2.2)`: the build time in UTC |
| Not a git checkout | The build time and a random hash |

- **Fetch tags first:** run `git fetch --tags` so the latest tag is there.
- **Set it yourself:** build with `OCD_BUILD_VERSION`, for example
  `OCD_BUILD_VERSION=1.0.0+$(git rev-parse --short HEAD) cargo build --release -p ocd`.
- **Tag format:** release tags must be `vMAJOR.MINOR.PATCH`, such as `v0.2.2`. `ocd upgrade` and the update check
  ignore anything else.

## Include the web UI

The web UI is upstream's web app, about 24 MB, so it isn't committed. Without it, `ocd serve` answers 404 for web
pages, the same as an upstream build made with `--skip-web-ui`. The TUI and CLI don't need it.

To include it, build it from an upstream checkout at `v2.2.2` with Bun 1.4.4, then rebuild ocd. The archive is
embedded at compile time. If `node` is on your `PATH`, it must be Node 22.12 or newer (or 20.19+), because the
app's bundler runs under it.

```sh
git clone https://github.com/anomalyco/opencode.git ../opencode
(cd ../opencode && git checkout v2.2.2 && bun install)
BUN=bun scripts/build-web-ui.sh ../opencode     # writes crates/ocd/web-ui/app-archive.bin
cargo build --release -p ocd
```

At runtime ocd looks for the web UI in this order:

1. the `OCD_WEB_UI` environment variable (an archive file or an unpacked `dist` folder);
2. the archive embedded in the binary;
3. an `app-archive.bin` file next to the binary.

## Rebuild the plugin helper

`crates/oc-plugin/host/dist/host.js` is committed, so building ocd never needs Bun. If you change the helper's
TypeScript (`crates/oc-plugin/host/src/`), rebuild the bundle with Bun 1.4.4. You need an upstream checkout at
`v2.2.2` with `bun install` run:

```sh
BUN=/path/to/bun-1.4.4 scripts/build-plugin-host.sh ../opencode
```

## Testing

```sh
cargo test --workspace --no-fail-fast
```

Some tests need the upstream checkout, the upstream binary or Bun 1.4.4. They find these in a working folder named
by `OCD_SCRATCH`, and are skipped when it isn't set:

```
$OCD_SCRATCH/src/opencode                         upstream checkout (tag v2.2.2, `bun install` done)
$OCD_SCRATCH/up-dist/cli-linux-x64/bin/opencode   upstream binary built from that checkout
$OCD_SCRATCH/bun144/bun-linux-x64/bun             Bun 1.4.4
```

```sh
export OCD_SCRATCH=$HOME/ocd-scratch
export OCD_BUN_PATH=$OCD_SCRATCH/bun144/bun-linux-x64/bun
cargo test --workspace --no-fail-fast
python3 -B scripts/parity.py -q              # side-by-side comparison with upstream (~25 min)
```

- **Keep real data safe:** run tests and parity runs with isolated `XDG_*_HOME` folders and `OPENCODE_TEST_HOME`,
  so they never touch your opencode data. [PORTING.md](PORTING.md) has the details.
- **Upgrade tests:** `crates/ocd/tests/upgrade.rs` points copies of the binary at a local stand-in for GitHub with
  `OCD_TEST_RELEASES_API`. It sets the build they report with `OCD_TEST_BUILD_VERSION`, which is only honoured
  together with the first; both are for tests only.
- **Replaying a real release:** with `$OCD_SCRATCH/releases/v0.1.1/` holding that release's `release.json` (the
  API's answer), `SHA256SUMS` and Linux x86_64 archive, the upgrade tests also replay it.

## Releases

Push a three-part `v*` tag to publish a release:

```sh
git tag v1.0.0
git push origin v1.0.0
```

| Workflow | What it does |
|---|---|
| [`release.yml`](../.github/workflows/release.yml) | Builds the web UI once, then ocd for linux-x64, linux-arm64, darwin-arm64 and windows-x64 (about 15 minutes), and publishes them with `SHA256SUMS` on the tag's release. The binaries report `<tag>+<commit>` |
| [`release-glibc2.17.yml`](../.github/workflows/release-glibc2.17.yml) | Runs after `release.yml` succeeds. It builds the two Linux targets with cargo-zigbuild against glibc 2.17, checks that no newer glibc symbol is used, and adds `ocd-<tag>-linux-<arch>-glibc2.17.tar.gz` to the release and `SHA256SUMS`. These builds report the `-glibc2.17` platform, so `ocd upgrade` keeps them on that variant |

- **Rebuild an existing tag:** run either workflow from the Actions tab with the tag name. Re-running
  `release.yml` triggers `release-glibc2.17.yml` again afterwards.
- **If the web UI fails:** the binaries are built without it. The step's last output lines appear as an
  annotation on the run page.

## Repository layout

| Path | What it holds |
|---|---|
| `crates/ocd` | The `ocd` binary: CLI commands, serve and service modes, upgrade, web UI embedding |
| `crates/oc-core` | Sessions, the agent loop, permissions, agents, skills, MCP, plugins, worktrees, v1 import |
| `crates/oc-backend` | The HTTP API, on top of oc-core |
| `crates/oc-server` | HTTP/SSE server, auth, CORS, web UI serving, generated API types |
| `crates/oc-tui` | The terminal UI (ratatui), full and mini modes |
| `crates/oc-ai` | Model catalogue (models.dev), provider protocols, credentials and OAuth, AI SDK route |
| `crates/oc-tools` | Built-in tools, ripgrep, websearch, output truncation |
| `crates/oc-codemode` | JavaScript interpreter for the Code Mode `execute` tool |
| `crates/oc-plugin` | The Bun plugin helper (`host/`) and the Rust side of its protocol |
| `crates/oc-mcp` | MCP client: stdio, streamable HTTP, OAuth |
| `crates/oc-config` | Config schema, discovery, normalisation, policies |
| `crates/oc-storage` | SQLite database, migrations, event projectors |
| `crates/oc-schema` | IDs, messages, session events |
| `crates/oc-git` | Git, snapshots, worktree strategy, VCS |
| `crates/oc-tunnel` | OpenTunnel client for `pair --remote` |
| `crates/oc-persistent-pty` | Client for the `opencode-pty` daemon |
| `crates/oc-util` | Paths, logging, JSONC, file locks, tracing headers |
| `assets/` | Verbatim upstream assets: prompts, themes, models.dev snapshot, OpenAPI document, SQL migrations |
| `docs/` | These guides, the specifications (`spec/`), parity results, porting rules and status |
| `scripts/` | Install script, parity suite, mock provider, web UI and plugin helper builds, sandbox launcher |

Most crates have a `README.md` with their API and any known differences from upstream.

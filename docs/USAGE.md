# Using ocd

[← README](../README.md)

ocd behaves like opencode 2.2.2, so [opencode's documentation](https://opencode.ai/docs) applies too. This page
covers what's specific to ocd.

- [Commands](#commands)
- [The background service](#the-background-service)
- [Try it safely](#try-it-safely)
- [Environment variables](#environment-variables)
- [Certificates and proxies](#certificates-and-proxies)
- [Downloads on first use](#downloads-on-first-use)
- [Troubleshooting](#troubleshooting)

## Commands

| Command | What it does |
|---|---|
| `ocd` | The TUI; starts the background server if it isn't running |
| `ocd --standalone` | The TUI with a private server that exits with it |
| `ocd run "prompt" -m provider/model` | One run without the TUI |
| `ocd serve` | The HTTP API and web UI in the foreground |
| `ocd auth login` | Sign in to a provider (the TUI's `/connect` does the same) |
| `ocd service status\|start\|stop\|restart` | Manage the background server |
| `ocd pair --remote` | Remote access to this machine through OpenTunnel |
| `ocd upgrade` | Install the latest release ([details](INSTALL.md#upgrading)) |
| `ocd --help` | Every command and option |

**`ocd run` reads extra prompt text from stdin** when stdin isn't a terminal, as upstream does. In scripts, pass
`</dev/null` if you have nothing to pipe in.

## The background service

The TUI starts a background server, which keeps running after you quit, as upstream's does. Later `ocd` runs and
web UI tabs connect to it instantly.

- **Address:** port 49374 (28894 on Windows), like upstream 2.1.
- **Stop it:** `ocd service stop`.
- **Never start it:** `ocd service set disabled true`. Every `ocd` then behaves like `--standalone`.
- **Restart it:** after copying a new binary in place, run `ocd service restart`. The installer and `ocd upgrade`
  restart it for you.

## Try it safely

`scripts/ocd-sandbox` runs `target/release/ocd` with its own config, data, state and cache folders under
`~/.local/share/ocd-sandbox`. It never touches your opencode data:

```sh
scripts/ocd-sandbox auth login
scripts/ocd-sandbox --standalone
```

## Environment variables

Every upstream `OPENCODE_*` variable works as it does upstream. These are specific to ocd:

| Variable | Purpose |
|---|---|
| `OCD_BUN_PATH` (or `OPENCODE_BUN_PATH`) | Use this Bun binary for plugins and AI SDK packages instead of downloading one |
| `OCD_BUN_MIRROR` | Download Bun from this mirror |
| `OCD_PLUGIN_HOST_JS` | Use this plugin helper bundle instead of the embedded one |
| `OCD_WEB_UI` | Serve the web UI from this archive or `dist` folder |
| `GITHUB_TOKEN` (or `GH_TOKEN`) | Sent to the GitHub API by `ocd upgrade` and the update check, for a higher rate limit |

## Certificates and proxies

ocd trusts the same certificates as upstream's runtime (Bun 1.4.4 with `--use-system-ca`), for HTTPS and
WebSockets:

| Source | Details |
|---|---|
| Mozilla root list | Built in |
| System store | The usual bundle files (such as `/etc/ssl/certs/ca-certificates.crt`) and certificate folders (such as `/etc/ssl/certs`). `SSL_CERT_FILE` replaces the bundle files; `SSL_CERT_DIR` (a `:`-separated list) replaces the folders |
| `NODE_EXTRA_CA_CERTS` | The PEM certificates in that file |

**Behind a TLS-inspecting proxy, or with a provider on a private certificate authority,** set
`NODE_EXTRA_CA_CERTS` to the authority's certificate file.

- **Unreadable file:** if that file can't be read or holds no certificate, the server prints
  `warn: ignoring extra certs from <file>, load failed: <reason>` and carries on without it, as upstream does.
  ocd also writes the line to its log.
- **Turning checks off:** `NODE_TLS_REJECT_UNAUTHORIZED=0` turns certificate checks off.

**Proxies:** outgoing requests use `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` (either case) with
upstream's rules. The OpenAI WebSocket transport also reads `WSS_PROXY` and `WS_PROXY`, and tunnels through `http:`
or `https:` proxies, with credentials taken from the proxy URL.

## Downloads on first use

When they're needed, ocd downloads these into the opencode cache folder, the same way upstream does:

| Download | When |
|---|---|
| `ripgrep` | glob and grep, if `rg` isn't installed |
| Bun 1.4.4 | Plugins or AI SDK packages only (or set `OCD_BUN_PATH`) |
| `opencode-pty` helper | Persistent terminals |
| npm packages | Plugins and AI SDK providers |

## Troubleshooting

Logs are in `~/.local/share/opencode/log/opencode.log`.

| Problem | What to do |
|---|---|
| A run ended before its tasks were done | `sh scripts/why-stopped.sh ses_…` shows, read-only, how the session's last runs ended: interrupted, model finished, or failed with a message |
| A process crashed | Every panic is logged as `panicked` with its message and source line: `grep panicked ~/.local/share/opencode/log/opencode.log` |
| The background service stopped | An open TUI starts it again by itself; `/restart` restarts it by hand. A stop by signal is logged as `server stopping signal=SIGTERM` (or SIGHUP, SIGINT); if that line is missing, it was killed with SIGKILL |
| The service dies when you log out of SSH | If `loginctl show-user "$USER" -p KillUserProcesses` says `yes`, run `loginctl enable-linger "$USER"` |
| The terminal is garbled after the TUI died | On a crash or a catchable signal, the TUI restores the terminal before exiting, like upstream. SIGKILL (for example from the out-of-memory killer) can't be caught: type `reset` and press Enter |
| `ocd` still runs an old version after an upgrade | `command -v ocd` shows which copy runs; remove the older one from your `PATH` |

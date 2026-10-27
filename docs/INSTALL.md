# Install and upgrade

[← README](../README.md)

- [Install script](#install-script)
- [Release files](#release-files)
- [Manual install](#manual-install)
- [Upgrading](#upgrading)
- [Update checks in the TUI](#update-checks-in-the-tui)
- [Versions](#versions)

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash
# or
wget -qO- https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash
```

[`scripts/install.sh`](../scripts/install.sh) runs as your user, with no root needed. It:

1. **Picks the release file** for your OS and CPU. On Linux with glibc older than 2.35 (RHEL/CentOS 7 and 8,
   Ubuntu 20.04, Debian 11 and so on) it takes the glibc 2.17 build.
2. **Checks the download** against the release's `SHA256SUMS` and runs the new binary once.
3. **Installs it** as `~/.local/bin/ocd`, replacing an older copy.
4. **Adds `~/.local/bin` to your `PATH`** if it isn't there yet, in your shell's startup file (`~/.bashrc`,
   `~/.zshrc`, fish's `config.fish`, or `~/.profile`). Open a new shell afterwards.
5. **Restarts the background service** (`ocd service restart`) on the new binary if ocd was already installed.

Options go after `bash -s --`, or set the environment variable instead:

```sh
curl -fsSL https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash -s -- --version v0.2.2
```

| Option | Variable | Effect |
|---|---|---|
| `--version <tag>` | `OCD_VERSION` | Install that release instead of the latest |
| `--dir <dir>` | `OCD_INSTALL_DIR` | Install into `<dir>` instead of `~/.local/bin` |
| `--no-modify-path` | `OCD_NO_MODIFY_PATH=1` | Leave shell startup files alone |
| `--no-restart` | `OCD_NO_RESTART=1` | Don't restart the background service |

If another `ocd` comes earlier on your `PATH` (for example an old `/usr/bin/ocd`), the script warns you. Remove
the old copy so `ocd` runs the new one.

## Release files

Every [release](https://github.com/sphinxid/ocd/releases) has these files, plus `SHA256SUMS`:

| File | Runs on |
|---|---|
| `ocd-<tag>-linux-x64.tar.gz` | Linux x86_64, glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later) |
| `ocd-<tag>-linux-arm64.tar.gz` | Linux ARM64, glibc 2.35 or newer |
| `ocd-<tag>-linux-x64-glibc2.17.tar.gz` | Linux x86_64, glibc 2.17 or newer (RHEL/CentOS 7, Ubuntu 14.04, Debian 8 and later) |
| `ocd-<tag>-linux-arm64-glibc2.17.tar.gz` | Linux ARM64, glibc 2.17 or newer |
| `ocd-<tag>-darwin-arm64.tar.gz` | macOS on Apple silicon |
| `ocd-<tag>-windows-x64.zip` | Windows x86_64 |

The glibc 2.17 files are the same code linked against an older glibc. They are added a few minutes after the
others. Musl-based systems such as Alpine aren't supported. Every binary includes the web UI.

## Manual install

1. Download the file for your platform and `SHA256SUMS` from the release.
2. Check it: `sha256sum -c SHA256SUMS --ignore-missing` (on macOS, `shasum -a 256 -c SHA256SUMS --ignore-missing`).
3. Unpack it and put `ocd-<tag>-<platform>/ocd` (or `ocd.exe`) somewhere on your `PATH`.
4. **macOS only:** the binary isn't signed. Clear the download quarantine once with
   `xattr -d com.apple.quarantine ocd`. The install script does this for you.

## Upgrading

```sh
ocd upgrade            # the latest release
ocd upgrade 0.2.0      # a given release, also an older one
```

`ocd upgrade` works however ocd was installed:

- **It verifies before replacing.** It downloads the release file for this build's platform, checks it against
  `SHA256SUMS` and runs the new binary once. Then it replaces its own executable.
- **It needs write access to its folder.** The new file is written next to the old one, so for `/usr/local/bin`,
  run it with `sudo`.
- **It handles the background service.** A running service is restarted on the new version; a stopped one stays
  stopped.
- **It keeps the build variant.** A glibc 2.17 build upgrades to the glibc 2.17 build of the new release.
- **A local build asks first.** A build not made from a release tag asks before replacing itself; pass `--yes`
  when there's no terminal to ask in.
- **`--method` is ignored.** It's accepted for upstream compatibility, but ocd always installs from its own
  releases.
- **Rate limits:** set `GITHUB_TOKEN` (or `GH_TOKEN`) if GitHub's limit of 60 API requests an hour gets in the way.

## Update checks in the TUI

The TUI looks for a newer release at most once a day. Upstream asks its update service every 10 minutes.

| Global config | Behaviour |
|---|---|
| (default) or `"autoupdate": "notify"` | Shows `/update to install vX` under the logo, like upstream |
| `"autoupdate": true` or `"update": "auto"` | Installs it on its own, but never restarts the service by itself |
| `"autoupdate": false`, or `OPENCODE_DISABLE_AUTOUPDATE=1` | No checks |

`/update` in the TUI checks and installs like `ocd upgrade`, but leaves the service alone. Afterwards, run
`ocd service restart` or `/restart`. Only a newer release counts (upstream offers any other version). A local build
never checks.

## Versions

`ocd --version` prints ocd's own version next to the opencode version it's compatible with:

```
ocd v0.2.2+1a2b3c4 (opencode 2.1.4)
```

`0.2.2` is the release tag and `1a2b3c4` the commit. Providers, plugins and the background service see opencode
2.1.4 on the `latest` channel, so they treat ocd like upstream. Local builds are described in
[BUILDING.md](BUILDING.md#version-numbers).

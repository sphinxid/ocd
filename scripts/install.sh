#!/usr/bin/env bash
# Installs ocd for the current user from the latest GitHub release (no root needed):
#
#   curl -fsSL https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash
#   wget -qO- https://raw.githubusercontent.com/sphinxid/ocd/main/scripts/install.sh | bash
#
# It picks the archive for this OS and CPU, checks it against the release's SHA256SUMS, and puts the
# binary in ~/.local/bin (adding that to PATH in your shell's startup file if it isn't there yet).
# If ocd was already installed, the background service is restarted on the new binary.
#
# Settings (environment variables, or flags after `bash -s --`):
#   OCD_VERSION=v0.2.1        --version v0.2.1   install this release tag instead of the latest
#   OCD_INSTALL_DIR=<dir>     --dir <dir>        install there instead of ~/.local/bin
#   OCD_NO_MODIFY_PATH=1      --no-modify-path   don't touch shell startup files
#   OCD_NO_RESTART=1          --no-restart       don't restart the service
#
# Everything runs from main() at the bottom, so a download cut short runs nothing.

set -euo pipefail

REPO="sphinxid/ocd"

info() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# fetch <url> <file>
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --tries=3 -O "$2" "$1"
  else
    die "curl or wget is needed to download ocd"
  fi
}

# The tag `releases/latest` redirects to (no API call, so no API rate limit).
latest_tag() {
  local url=""
  if command -v curl >/dev/null 2>&1; then
    url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" || true
  elif command -v wget >/dev/null 2>&1; then
    url="$(wget -q -S --spider --max-redirect=0 "https://github.com/$REPO/releases/latest" 2>&1 |
      sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r')" || true
  fi
  case "$url" in
    */releases/tag/*) printf '%s\n' "${url##*/releases/tag/}" ;;
    *) die "could not find the latest release of $REPO (set OCD_VERSION to pick one)" ;;
  esac
}

# version_at_least <version> <minimum>: compares dotted major.minor versions.
version_at_least() {
  local a_major a_minor b_major b_minor
  a_major="${1%%.*}"
  a_minor="${1#*.}"
  a_minor="${a_minor%%.*}"
  b_major="${2%%.*}"
  b_minor="${2#*.}"
  [ "$a_major" -gt "$b_major" ] || { [ "$a_major" -eq "$b_major" ] && [ "$a_minor" -ge "$b_minor" ]; }
}

# The release's platform name for this machine: linux-x64, linux-arm64 (or either with -glibc2.17 on
# glibc older than 2.35) or darwin-arm64.
detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch="x64" ;;
    aarch64 | arm64) arch="arm64" ;;
    *) die "no ocd release for CPU $arch (there are x64 and arm64 builds)" ;;
  esac
  case "$os" in
    Linux)
      # musl's ldd prints its name and exits 1 for --version.
      if [ -f /etc/alpine-release ] || case "$(ldd --version 2>&1 || true)" in *musl*) true ;; *) false ;; esac; then
        die "the Linux builds need glibc; this system uses musl (Alpine?)"
      fi
      # The regular Linux builds need glibc 2.35; older systems get the glibc 2.17 build.
      local glibc
      glibc="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
      glibc="${glibc##* }"
      if [ -n "$glibc" ] && ! version_at_least "$glibc" 2.35; then
        version_at_least "$glibc" 2.17 || die "glibc $glibc is too old; ocd needs glibc 2.17 or newer"
        printf 'linux-%s-glibc2.17\n' "$arch"
      else
        printf 'linux-%s\n' "$arch"
      fi
      ;;
    Darwin)
      # A shell running under Rosetta reports x86_64 on Apple silicon.
      if [ "$arch" = "x64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
        arch="arm64"
      fi
      [ "$arch" = "arm64" ] || die "there is no Intel macOS build; ocd runs on Apple silicon Macs"
      printf 'darwin-arm64\n'
      ;;
    MINGW* | MSYS* | CYGWIN*)
      die "on Windows, download ocd-<tag>-windows-x64.zip from https://github.com/$REPO/releases/latest"
      ;;
    *) die "no ocd release for $os" ;;
  esac
}

# sha256 <file>: the file's hex SHA-256.
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    die "sha256sum or shasum is needed to check the download"
  fi
}

# The startup file a login or interactive shell of the user's shell reads.
shell_rc() {
  case "$(basename "${SHELL:-}")" in
    zsh) printf '%s\n' "${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash)
      if [ "$(uname -s)" = "Darwin" ]; then
        printf '%s\n' "$HOME/.bash_profile"
      else
        printf '%s\n' "$HOME/.bashrc"
      fi
      ;;
    fish) printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
    *) printf '%s\n' "$HOME/.profile" ;;
  esac
}

# add_to_path <dir>: makes new shells find <dir>; returns 1 if it changed a startup file.
add_to_path() {
  local dir="$1" rc shown line
  case ":$PATH:" in *":$dir:"*) return 0 ;; esac
  # Write the directory relative to $HOME when it's under it.
  shown="$dir"
  case "$dir" in "$HOME"/*) shown="\$HOME/${dir#"$HOME"/}" ;; esac
  rc="$(shell_rc)"
  case "$rc" in
    */config.fish) line="fish_add_path \"$shown\"" ;;
    *) line="export PATH=\"$shown:\$PATH\"" ;;
  esac
  if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
    info "$rc already adds $dir to PATH; open a new shell to pick it up"
    return 1
  fi
  mkdir -p "$(dirname "$rc")"
  printf '\n# ocd\n%s\n' "$line" >>"$rc"
  info "added $dir to PATH in $rc"
  return 1
}

main() {
  local version="${OCD_VERSION:-}"
  local dir="${OCD_INSTALL_DIR:-$HOME/.local/bin}"
  local modify_path=1 restart=1
  [ -z "${OCD_NO_MODIFY_PATH:-}" ] || modify_path=0
  [ -z "${OCD_NO_RESTART:-}" ] || restart=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --version) version="${2:?--version needs a tag}"; shift 2 ;;
      --dir) dir="${2:?--dir needs a directory}"; shift 2 ;;
      --no-modify-path) modify_path=0; shift ;;
      --no-restart) restart=0; shift ;;
      -h | --help)
        info "usage: install.sh [--version <tag>] [--dir <dir>] [--no-modify-path] [--no-restart]"
        return 0
        ;;
      *) die "unknown option $1" ;;
    esac
  done
  [ -n "${HOME:-}" ] || die "HOME is not set"
  case "$dir" in /*) ;; *) dir="$PWD/$dir" ;; esac

  local platform tag
  platform="$(detect_platform)"
  if [ -n "$version" ]; then
    case "$version" in v*) tag="$version" ;; *) tag="v$version" ;; esac
  else
    tag="$(latest_tag)"
  fi

  # Whether ocd was installed before (here or anywhere on PATH): then the service gets restarted.
  local dest="$dir/ocd" previous=""
  if [ -x "$dest" ]; then
    previous="$dest"
  elif command -v ocd >/dev/null 2>&1; then
    previous="$(command -v ocd)"
  fi

  local name="ocd-$tag-$platform"
  local base="https://github.com/$REPO/releases/download/$tag"
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/ocd-install.XXXXXX")"
  # shellcheck disable=SC2064 # expand $tmp now
  trap "rm -rf '$tmp'" EXIT

  info "downloading ocd $tag for $platform"
  if ! fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz"; then
    case "$platform" in
      *-glibc2.17) die "could not download $base/$name.tar.gz (releases before the glibc 2.17 builds don't have it)" ;;
      *) die "could not download $base/$name.tar.gz" ;;
    esac
  fi
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS"
  local want got
  want="$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
  [ -n "$want" ] || die "SHA256SUMS has no entry for $name.tar.gz"
  got="$(sha256 "$tmp/$name.tar.gz")"
  [ "$want" = "$got" ] || die "checksum mismatch for $name.tar.gz (expected $want, got $got)"

  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
  local new="$tmp/$name/ocd"
  [ -f "$new" ] || die "$name.tar.gz has no $name/ocd"
  chmod 755 "$new"
  if [ "$platform" = "darwin-arm64" ]; then
    xattr -d com.apple.quarantine "$new" 2>/dev/null || true
  fi
  local reported
  reported="$("$new" --version </dev/null 2>&1)" || die "the downloaded binary does not run here: $reported"

  # Copy next to the destination, then rename over it: a running ocd keeps its old file.
  mkdir -p "$dir"
  cp "$new" "$dest.new.$$"
  chmod 755 "$dest.new.$$"
  mv -f "$dest.new.$$" "$dest"
  info "installed $reported to $dest"

  local path_changed=0
  if [ "$modify_path" = 1 ]; then
    add_to_path "$dir" || path_changed=1
  fi

  # Another ocd earlier on PATH would still be the one `ocd` runs.
  local resolved
  resolved="$(command -v ocd 2>/dev/null || true)"
  if [ -n "$resolved" ] && [ "$resolved" != "$dest" ]; then
    case ":$PATH:" in
      *":$dir:"*) warn "\`ocd\` still runs $resolved, which comes before $dir on PATH; remove it or reorder PATH" ;;
      *) [ "$path_changed" = 1 ] || warn "\`ocd\` runs $resolved; $dir is not on PATH" ;;
    esac
  fi

  if [ -n "$previous" ] && [ "$restart" = 1 ]; then
    info "restarting the ocd service on the new binary"
    if "$dest" service restart </dev/null; then
      :
    else
      warn "ocd service restart failed; run '$dest service restart' yourself"
    fi
  fi

  if [ "$path_changed" = 1 ]; then
    info ""
    info "open a new shell to use ocd (the new PATH applies to shells started from now on)"
  fi
}

main "$@"

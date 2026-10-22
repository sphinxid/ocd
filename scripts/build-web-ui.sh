#!/usr/bin/env bash
# Builds the upstream web UI (packages/app) and packs it into the archive ocd embeds
# (crates/ocd/web-ui/app-archive.bin, not committed).
#
# Usage: scripts/build-web-ui.sh <upstream-opencode-checkout> [output]
#   (or set OPENCODE_UPSTREAM). The checkout must be the pinned upstream commit with its
#   dependencies installed (`bun install` at its root). BUN selects the bun binary (default: `bun`
#   on PATH; upstream pins bun 1.4.2). OPENCODE_CHANNEL selects the channel the app is built for
#   (default `latest`, ocd's default channel, which the app reads as `prod`).
#
# Like upstream's `packages/cli/script/build.ts`, this runs the app's own `vite build`, which writes
# `packages/app/dist` in the checkout (git-ignored there), then packs every file but source maps
# with brotli quality 11 in upstream's archive format (`packages/cli/src/app-archive.ts`).
#
# Rebuild ocd afterwards (`cargo build -p ocd`): the archive is embedded at compile time. A binary
# built without it serves no web UI (404 for non-API paths), like an upstream `--skip-web-ui` build.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="${1:-${OPENCODE_UPSTREAM:-}}"
output="${2:-$root/crates/ocd/web-ui/app-archive.bin}"
bun="${BUN:-bun}"
channel="${OPENCODE_CHANNEL:-latest}"

if [ -z "$upstream" ]; then
  echo "usage: $0 <upstream-opencode-checkout> [output]" >&2
  exit 2
fi
upstream="$(cd "$upstream" && pwd)"
if [ ! -f "$upstream/packages/app/package.json" ] || [ ! -f "$upstream/packages/cli/script/app-assets.ts" ]; then
  echo "error: $upstream does not look like an opencode v2 checkout (packages/app or packages/cli missing)" >&2
  exit 2
fi
if [ ! -d "$upstream/node_modules" ]; then
  echo "error: run 'bun install' in $upstream first" >&2
  exit 2
fi

# `vite build` runs under `node` when one is on PATH (bun stands in for it otherwise), and the
# app's bundler (rolldown) needs Node ^20.19 or >=22.12.
if command -v node >/dev/null 2>&1; then
  node_version="$(node --version)"
  if ! node -e 'const [a, b] = process.versions.node.split(".").map(Number); process.exit(a > 22 || (a === 22 && b >= 12) || (a === 20 && b >= 19) ? 0 : 1)'; then
    echo "error: node $node_version on PATH is too old for the web UI build (needs ^20.19 or >=22.12)" >&2
    exit 2
  fi
fi

# The app build calls `bun` by name; make it the selected one.
bun="$(command -v "$bun")"
export PATH="$(dirname "$bun"):$PATH"
# Never upload source maps from a local build.
unset SENTRY_AUTH_TOKEN SENTRY_ORG SENTRY_PROJECT

mkdir -p "$(dirname "$output")"
tmp="$output.tmp.$$"
trap 'rm -f "$tmp"' EXIT
echo "building web UI with bun $("$bun" --version) (channel $channel) from $upstream"
(cd "$upstream" && "$bun" "$root/scripts/build-web-ui.ts" "$upstream" "$tmp" "$channel")

# The archive is embedded in the binary: it must not carry local paths.
for needle in "$upstream" "$root" "$HOME"; do
  if grep -rlaF --exclude='*.map' "$needle" "$upstream/packages/app/dist" >&2; then
    echo "error: the files above mention $needle" >&2
    exit 1
  fi
done
mv "$tmp" "$output"
trap - EXIT
echo "web UI archive: $output ($(wc -c <"$output") bytes)"

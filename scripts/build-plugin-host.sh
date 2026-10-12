#!/usr/bin/env bash
# Rebuilds the ocd plugin sidecar bundle (crates/oc-plugin/host/dist/host.js).
#
# Usage: scripts/build-plugin-host.sh <upstream-opencode-checkout>
#   (or set OPENCODE_UPSTREAM). The checkout must be the pinned upstream commit with its
#   dependencies installed (`bun install` at its root). BUN selects the bun binary used to
#   bundle (default: `bun` on PATH; upstream pins bun 1.4.2).
#
# The bundle is committed; building ocd itself never needs bun or the upstream checkout.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="${1:-${OPENCODE_UPSTREAM:-}}"
bun="${BUN:-bun}"

if [ -z "$upstream" ]; then
  echo "usage: $0 <upstream-opencode-checkout>" >&2
  exit 2
fi
if [ ! -f "$upstream/packages/plugin/package.json" ]; then
  echo "error: $upstream does not look like an opencode checkout (packages/plugin missing)" >&2
  exit 2
fi
if [ ! -d "$upstream/node_modules" ]; then
  echo "error: run 'bun install' in $upstream first" >&2
  exit 2
fi

host="$root/crates/oc-plugin/host"
echo "bundling with $("$bun" --version) against $upstream"
"$bun" "$host/build.ts" "$upstream" "$host/dist"

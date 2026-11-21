#!/usr/bin/env bash
# Rebuilds the ocd plugin sidecar bundle (crates/oc-plugin/host/dist/host.js).
#
# Usage: scripts/build-plugin-host.sh <upstream-opencode-checkout>
#   (or set OPENCODE_UPSTREAM). The checkout must be at the pinned upstream commit
#   (UPSTREAM_COMMIT in .github/workflows/release.yml) with its dependencies installed
#   (`bun install --frozen-lockfile` at its root). BUN selects the bun binary used to bundle
#   (default: `bun` on PATH); it should be the pinned BUN_VERSION from the same file.
#
# The bundle is reproducible: the same pin and bun give the same bytes, which the
# `Plugin helper` workflow checks against the committed file.
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

pin() { sed -n "s/^  $1: *\"\{0,1\}\([^\" ]*\)\"\{0,1\}.*/\1/p" "$root/.github/workflows/release.yml" | head -n 1; }
commit="$(pin UPSTREAM_COMMIT)"
bun_version="$(pin BUN_VERSION)"
if [ -z "$commit" ] || [ -z "$bun_version" ]; then
  echo "error: could not read UPSTREAM_COMMIT and BUN_VERSION from .github/workflows/release.yml" >&2
  exit 2
fi
head="$(git -C "$upstream" rev-parse HEAD 2>/dev/null || true)"
if [ "$head" != "$commit" ]; then
  echo "error: $upstream is at ${head:-an unknown commit}, not the pinned upstream commit $commit" >&2
  exit 2
fi
if [ "$("$bun" --version)" != "$bun_version" ]; then
  echo "warning: bun $("$bun" --version) is not the pinned $bun_version; the bundle may differ from the committed one" >&2
fi

host="$root/crates/oc-plugin/host"
echo "bundling with $("$bun" --version) against $upstream"
"$bun" "$host/build.ts" "$upstream" "$host/dist"
if command -v sha256sum >/dev/null; then
  sha256sum "$host/dist/host.js"
else
  shasum -a 256 "$host/dist/host.js"
fi

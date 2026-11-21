#!/usr/bin/env bash
# Checks that the committed plugin sidecar bundle is the one the pinned upstream checkout and Bun
# produce.
#
# Usage: scripts/check-plugin-host.sh <upstream-opencode-checkout>
#   (or set OPENCODE_UPSTREAM). The checkout must be upstream's v2.3.1 tag (commit c769de8) with
#   its dependencies installed (`bun install` at its root). BUN selects the bun binary used to
#   bundle (default: `bun` on PATH; upstream v2.3.1 pins bun 1.4.4).
#
# This is the check CI runs. It builds into a temporary directory and compares with
# crates/oc-plugin/host/dist/host.js, so it never writes the committed file.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="${1:-${OPENCODE_UPSTREAM:-}}"
bundle="$root/crates/oc-plugin/host/dist/host.js"

if [ -z "$upstream" ]; then
  echo "usage: $0 <upstream-opencode-checkout>" >&2
  exit 2
fi
if [ ! -f "$bundle" ]; then
  echo "error: $bundle is missing" >&2
  exit 2
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/ocd-plugin-host.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

"$root/scripts/build-plugin-host.sh" "$upstream" "$tmp"

if cmp -s "$bundle" "$tmp/host.js"; then
  echo "plugin host: committed bundle matches the rebuild"
  echo "  $(sha256sum "$bundle")"
  exit 0
fi

echo "error: the committed plugin host bundle differs from the rebuild." >&2
echo "  committed: $(sha256sum "$bundle")" >&2
echo "  rebuilt:   $(sha256sum "$tmp/host.js")" >&2
cmp "$bundle" "$tmp/host.js" 2>&1 | head -1 >&2 || true
cat >&2 <<'MSG'

If the helper's TypeScript or the pinned upstream commit changed intentionally, rebuild the
committed bundle and audit the result before committing it:

  scripts/build-plugin-host.sh <upstream-opencode-checkout>

The bundle is minified JavaScript that no type checker covers; the rebuild is what proves that
the committed file is the one this source produces.
MSG
exit 1

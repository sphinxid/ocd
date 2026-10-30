#!/usr/bin/env bash
# Regenerates tests/fixtures/upstream from upstream's codemode test suite (see tests/fixtures/upstream/README.md).
#
#   record.sh UPSTREAM CORPUS WORK [OUT]
#
# UPSTREAM  an opencode checkout at the tag being tracked, with `bun install` done (node_modules present)
# CORPUS    a directory holding the vendored test262 `built-ins/` and `language/` trees (upstream's
#           `bun run script/sync-test262.ts` output, at the revision in test/test262/manifest.json)
# WORK      an empty scratch directory for the instrumented package copy and the logs
# OUT       where the full recording goes (default: WORK/out); install it with install.py
# BUN       the bun binary (default: bun on PATH)
#
# The instrumented copy is upstream's packages/codemode plus this directory's files: src/record.ts (all encoding and
# output, inert unless CODEMODE_RECORD_DIR is set), src/tool-schema.ts (a recording shim over the original, renamed
# src/tool-schema-impl.ts), record-preload.ts (tracks the current test), script/record-extras.ts
# (search-signature.json), and hooks.patch (the recording calls in codemode.ts, tool-runtime.ts,
# interpreter/execute.ts, interpreter/extensions.ts, openapi/index.ts and openapi/runtime.ts).
set -euo pipefail

KIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UPSTREAM="$(cd "$1" && pwd)"
CORPUS="$(cd "$2" && pwd)"
WORK="$(mkdir -p "$3" && cd "$3" && pwd)"
OUT="${4:-$WORK/out}"
BUN="${BUN:-bun}"

PKG="$WORK/packages/codemode-rec"
rm -rf "$WORK/packages"
mkdir -p "$WORK/packages"
ln -sfn "$UPSTREAM/node_modules" "$WORK/node_modules"
cp -r "$UPSTREAM/packages/codemode" "$PKG"
rm -rf "$PKG/test/test262/built-ins" "$PKG/test/test262/language"
mv "$PKG/src/tool-schema.ts" "$PKG/src/tool-schema-impl.ts"
cp "$KIT/record.ts" "$KIT/tool-schema.ts" "$PKG/src/"
cp "$KIT/record-preload.ts" "$PKG/"
cp "$KIT/record-extras.ts" "$PKG/script/"
(cd "$PKG" && patch -p1 --quiet <"$KIT/hooks.patch")

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
find "$OUT" -mindepth 1 -maxdepth 1 ! -name README.md -exec rm -rf {} +
LOGS="$WORK/logs"
rm -rf "$LOGS"
mkdir -p "$LOGS"
summary() { grep -E '^ *[0-9]+ (pass|fail|skip|todo|error)|^Ran |^\(fail\)' "$1" || true; }

# Pass 1: the regular suite, without the test262 corpus (test/test262.test.ts then defines no tests).
echo "pass 1: bun test (recording) -> $OUT"
(cd "$PKG" && CODEMODE_RECORD_DIR="$OUT" "$BUN" test --preload ./record-preload.ts >"$LOGS/pass1.log" 2>&1) || true
summary "$LOGS/pass1.log"

# Pass 2: the vendored test262 corpus.
echo "pass 2: test262 (recording) from $CORPUS"
cp -r "$CORPUS/built-ins" "$CORPUS/language" "$PKG/test/test262/"
(cd "$PKG" && CODEMODE_RECORD_DIR="$OUT" "$BUN" test --preload ./record-preload.ts test/test262.test.ts \
  >"$LOGS/pass2.log" 2>&1) || true
rm -rf "$PKG/test/test262/built-ins" "$PKG/test/test262/language"
summary "$LOGS/pass2.log"

(cd "$PKG" && "$BUN" run script/record-extras.ts "$OUT")
echo "logs: $LOGS"
echo "records per file:"
(cd "$OUT" && find . -name '*.jsonl' | sed 's#^\./##' | sort | xargs wc -l | sed '$d')

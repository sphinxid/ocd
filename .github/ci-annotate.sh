#!/usr/bin/env bash
# Turns a failed test log (libtest or cargo-nextest output) into GitHub annotations, which are readable through the
# public API without access to the job logs: a per-binary count of failures, every failing test, then the panics,
# packed into as many annotations as GitHub keeps for one step (10 errors, 10 warnings and 10 notices of at most
# 4096 characters each).
#
# Usage: .github/ci-annotate.sh test.log
set -u
log=${1:-test.log}
[ -f "$log" ] || exit 0
limit=3900

# Failing tests, once each: libtest's `test x ... FAILED` and nextest's `FAIL [ 0.1s] ( 1/9) crate::bin test`
# (nextest repeats the latter in its final summary, without or with the counter).
failures=$( { grep -E '^test .* FAILED$' "$log" | sed -E 's/^test (.*) \.\.\. FAILED$/FAIL \1/'
              grep -E '^ +(FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|ABORT|LEAK|LEAK-FAIL) \[' "$log" \
                | sed -E 's/^ +([A-Z-]+) \[[^]]*\] +(\( *[0-9]+\/[0-9]+\) +)?/\1 /'; } | awk '!seen[$0]++')
count=$(printf '%s\n' "$failures" | grep -c . || true)

# Escapes one annotation message for a workflow command.
esc() { local s=$1; s=${s//'%'/'%25'}; s=${s//$'\r'/}; s=${s//$'\n'/'%0A'}; printf '%s' "$s"; }

messages=()
# Per test binary: how many of its tests failed.
if [ "$count" -gt 0 ]; then
  per_binary=$(printf '%s\n' "$failures" | awk '{ n[$2]++ } END { for (b in n) print n[b], b }' \
               | sort -k2 | awk '{ printf "%s%s: %s", (NR > 1 ? ", " : ""), $2, $1 }')
  messages+=("$count failing: $per_binary")
fi
# Every failing test, as many annotations as it takes.
chunk=""
while IFS= read -r t; do
  [ -n "$t" ] || continue
  if [ $(( ${#chunk} + ${#t} + 1 )) -gt $limit ]; then messages+=("$chunk"); chunk=""; fi
  chunk="${chunk:+$chunk$'\n'}$t"
done <<< "$failures"
[ -n "$chunk" ] && messages+=("$chunk")
# Each panic with the lines that explain it (up to 14 lines and 1500 characters), several per annotation.
chunk=""
while IFS= read -r p; do
  [ -n "$p" ] || continue
  p=${p//$'\x1f'/$'\n'}
  if [ $(( ${#chunk} + ${#p} + 2 )) -gt $limit ]; then messages+=("$chunk"); chunk=""; fi
  chunk="${chunk:+$chunk$'\n\n'}$p"
done < <(awk '
  function flush() { if (msg != "") { if (length(msg) > 1500) msg = substr(msg, 1, 1500) "..."; print msg }; msg = "" }
  # A test nextest ended (TIMEOUT) or that crashed has no panic: the last lines of its output instead.
  function flush_tail(   i, out, from) {
    if (tail_name == "" || tail_n == 0) { tail_name = ""; tail_n = 0; return }
    from = tail_n > 12 ? tail_n - 12 : 0
    out = tail_name " (last output)"
    for (i = from; i < tail_n; i++) out = out "\x1f" tail[i]
    if (length(out) > 900) out = substr(out, length(out) - 900)
    print out
    tail_name = ""; tail_n = 0
  }
  /^ *(PASS|FAIL|TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|ABORT|LEAK) \[/ { flush(); flush_tail() }
  /^ *(TIMEOUT|SIGSEGV|SIGABRT|SIGKILL|ABORT) \[/ { tail_name = $0; sub(/^ +/, "", tail_name); next }
  tail_name != "" && /^(────|------------|Summary|     Summary)/ { flush_tail() }
  tail_name != "" && !/^ *(stdout|stderr) ───/ && NF > 0 { line = $0; sub(/^    /, "", line); tail[tail_n++] = line }
  /panicked at / { flush(); sub(/^ +/, ""); msg = $0; n = 0; next }
  msg != "" && (/^ *note: run with/ || /(stdout|stderr) ───/ || /^---- / || n >= 14) { flush(); next }
  msg != "" { line = $0; sub(/^    /, "", line); msg = msg "\x1f" line; n++ }
  END { flush(); flush_tail() }' "$log")
[ -n "$chunk" ] && messages+=("$chunk")

i=0
for m in "${messages[@]}"; do
  if [ $i -lt 10 ]; then kind=error; elif [ $i -lt 20 ]; then kind=warning; elif [ $i -lt 30 ]; then kind=notice; else break; fi
  [ $i -eq 0 ] && title="Failures by binary" || title="Failures $((i + 1))"
  echo "::$kind title=$title::$(esc "$m")"
  i=$((i + 1))
done

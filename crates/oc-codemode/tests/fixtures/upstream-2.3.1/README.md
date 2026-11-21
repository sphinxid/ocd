# Upstream CodeMode fixtures, opencode 2.3.1

The same kind of recording as `../upstream/` (format: `../upstream/README.md`, additions: `../upstream-2.3.0/README.md`),
made from opencode **v2.3.1** (`packages/codemode` at commit `c769de8300`) with Bun 1.4.4, after the language/Code Mode
split (#64487): the interpreter is `src/language/` and the tools, hooks and extensions are host functions that
`src/codemode/` gives it.

Regenerate with the recording kit in `tests/recorder/` (it targets v2.3.1's `codemode/{codemode,tool-runtime}.ts`,
`codemode/openapi/{index,runtime}.ts`, `language/runtime.ts` and `language/runtime/{execute,extensions}.ts`):

```sh
tests/recorder/record.sh OPENCODE_2_3_1 CORPUS WORK
python3 -I tests/recorder/install.py WORK/out tests/fixtures/upstream-2.3.1
rm -r tests/fixtures/upstream-2.3.1/test262 tests/fixtures/upstream-2.3.1/tests.jsonl
```

The instrumented suite gives upstream's own result except `language-boundary.test.ts` "nothing in the language layer
imports Code Mode" (the recording kit's calls in `language/` import the recorder). The test262 file
`language/global-code/import.js` fails upstream too.

Two things are left out of this directory: the test262 shards, which are byte-identical to `../upstream-2.3.0/test262/`
(the deterministic quarter, 3 340 of 13 330 executions, gives the same results on 2.3.1), and `tests.jsonl`, the
per-test counts no test reads.

What the recording shows of the split:

- Tool calls are recorded from `ToolRuntime.globals` (the run's `tools` and `search` host functions); a `decode`
  event also carries `json`, the argument as JSON after it crossed as a host copy (functions left out, promises
  awaited, a program's `toJSON` not run), which the replay's host compares with what ocd hands it.
- Extension calls are recorded from `CodeMode.make`'s `extensionFunction` (globals) and from the language's wrapper of
  a function inside a result (`global: false`). A host `Buffer` is recorded as `{"$buffer": base64}`. A function of an
  object global is labelled by its path (`fs.readSync`). `returned` is `"effect"` when the host function returned an
  Effect, which a synchronous global's caller waits for (#64363), and `"promise"` for a Promise; `extensions.test.jsonl`
  was re-recorded with that kit change (nothing else in it changed), and the other files still say `"promise"` for
  both (none of them has a synchronous global returning an Effect).
- `via` is `"language"` for runs of the language runtime itself (`language.test.ts`, the snapshot tests); the replay
  skips those, as it skips `program` and `resume`.
- A language result carries the run's `metadata`; the recording turns it into Code Mode's `toolCalls`.

## How the tests use it

`tests/upstream_replay.rs`, `tests/extensions_replay.rs`, `tests/hooks.rs` and `tests/signatures.rs` replay this
corpus next to `../upstream/` and `../upstream-2.3.0/`, which stay enforced: their records whose result 2.3.1
changed are listed in `../replay-skips.txt` and `../replay-skips-2.3.0.txt` with the 2.3.1 record that replaces them.
Records of this corpus ocd does not reproduce are listed in `../replay-skips-2.3.1.txt` and
`../extension-skips-2.3.1.txt`. `OC_CODEMODE_CORPUS=2.3.1` points every other test that reads `fixtures()` at it.

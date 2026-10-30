# Upstream CodeMode fixtures

Recordings of every program the upstream `@opencode/codemode` test suite (TypeScript, Effect 4) executes, for the
Rust replay harness of `oc-codemode`. The current set was recorded from upstream v2.2.2 with Bun 1.4.4. They are
generated, never hand-edited: run

```sh
tests/recorder/record.sh UPSTREAM CORPUS WORK [OUT]   # an opencode checkout with node_modules, the test262 corpus
python3 -I tests/recorder/install.py OUT tests/fixtures/upstream
```

`record.sh` builds `WORK/packages/codemode-rec`: a copy of upstream's `packages/codemode` with the recording kit in
`tests/recorder/` applied (`record.ts` holds all encoding and output and is inert unless `CODEMODE_RECORD_DIR` is set;
`tool-schema.ts` is a recording shim over the renamed `src/tool-schema-impl.ts`; `hooks.patch` adds the recording
calls to `codemode.ts`, `tool-runtime.ts`, `interpreter/execute.ts`, `interpreter/extensions.ts`, `openapi/index.ts`
and `openapi/runtime.ts`; `record-preload.ts` tracks the running test; `record-extras.ts` writes
`search-signature.json`). It runs the regular suite (pass 1), then the vendored test262 corpus (pass 2). Library
behaviour is unchanged: the instrumented suite has the same pass/fail result as upstream (2.2.2: 1 480 and 13 330
passing tests, no failures). `install.py` replaces everything here except this README and keeps the test262 sample
described at the end. `tests/recorder/stdlib-check.ts` re-checks the expectations of the `tests/stdlib_*.rs` parity
cases against upstream.

## Files

| File | One line per |
| --- | --- |
| `<test-file>.test.jsonl` (e.g. `parity.test.jsonl`) | program execution started from that test file |
| `test262/<built-ins\|language>/<Dir>.jsonl` | execution of one vendored test262 file (`test/test262.test.ts`), sharded by the file's first two path segments |
| `catalog.jsonl` | distinct read of `runtime.catalog` (deduplicated per test file + tree + catalog) |
| `schema-calls.jsonl` | direct call a test makes into `src/tool-schema.ts` (`inputTypeScript`, `jsonSchemaToTypeScript`, `decodeInput`, ...) |
| `make-errors.jsonl` | `CodeMode.make` / `CodeMode.execute` that threw synchronously (invalid limits, extension global clashes, empty tool-name segments) |
| `openapi-fromspec.jsonl` | `OpenAPI.fromSpec` call: the spec, options, resulting tool tree and `skipped` list |
| `openapi-http.jsonl` | HTTP round trip an OpenAPI tool performed, inside an execution or called directly via `tool.execute` |
| `tests.jsonl` | test body that ran, with how many records of each kind it produced |
| `search-signature.json` | the built-in `search` signature and `toolExpression` renderings |

Any `.jsonl` that would exceed 9 MB continues in `<name>.part2.jsonl`, `<name>.part3.jsonl`, and so on. Lines in an
execution file are written when the execution **finishes**, so they are not necessarily in `seq` order.

## Value encoding

Every host value is converted to JSON. Plain objects keep their own enumerable keys in insertion order; arrays stay
arrays. Values JSON cannot represent become single-key marker objects:

| Marker | Meaning |
| --- | --- |
| `{"$undefined": true}` | `undefined` (also used for an object field whose value is `undefined`) |
| `{"$number": "-0" \| "NaN" \| "Infinity" \| "-Infinity"}` | non-JSON numbers |
| `{"$hole": true}` | array hole |
| `{"$bigint": "123"}`, `{"$symbol": description}` | BigInt, Symbol |
| `{"$function": name}` | function (e.g. an extension result's method, a hook-visible function) |
| `{"$date": iso \| null}` | `Date` (`null` when invalid) |
| `{"$regexp": {"source", "flags"}}` | `RegExp` |
| `{"$bytes": base64}` | `Uint8Array` / `ArrayBuffer` |
| `{"$map": [[k, v], ...]}`, `{"$set": [v, ...]}` | `Map`, `Set` |
| `{"$url": href}`, `{"$urlsearchparams": "a=1"}`, `{"$headers": [[k, v], ...]}` | web built-ins |
| `{"$promise": true}` | a `Promise` |
| `{"$error": {"name", "message", "cause"?, ...own fields}}` | an `Error` (any subclass; `stack` omitted) |
| `{"$instance": className, "fields": {...}}` | a non-plain object (class instance), own enumerable fields |
| `{"$circular": true}` | a back-reference to an object being encoded |
| `{"$tooLarge": true}` | remainder of a value past the 200 000-node encoding budget (shared substructure is expanded) |
| `{"$throws": message}` | the computation that should have produced this value threw |
| `{"$unencodable": message}` | encoding itself threw (hostile getter etc.) |

Truncation: a whole line above 2 MB is re-serialized with strings over 20 000 chars replaced by
`{"$truncatedString": {"length", "prefix"}}` and arrays over 2 000 items cut to 100 items plus
`{"$truncatedArray": length}`, and gets a top-level `"truncated": true`. No current record needed this.

## Execution records (`<test-file>.test.jsonl`, `test262/**.jsonl`)

One object per run of the Effect returned by `runtime.execute(code)` (`CodeMode.make(...).execute`,
`CodeMode.execute(...)`) or by `executeProgram(...)` directly (test262 harness). Re-running the same Effect produces
another record. Fields, in order:

| Field | Description |
| --- | --- |
| `file` | test file basename, e.g. `"parity.test.ts"` |
| `test` | full test name: describe titles and the test title joined with `" > "`; `test.each` titles are formatted (`%s`, `%j`, `%p`, `%d`, `%i`, `%#`, `$field`). For test262 it is the vendored file path, e.g. `"built-ins/Array/from/source.js"`. Absent if the program ran outside any test body. |
| `seq` | 0-based order in which executions **started** within this test file (per process; test262 records share one counter) |
| `via` | `"codemode"` (through `CodeMode.make().execute`) or `"executeProgram"` (direct call; only test262) |
| `openapi` | `true` when any tool in `tree` came from `OpenAPI.fromSpec`; absent otherwise |
| `code` | program source exactly as given. test262 code is `"use strict";\n` + file source, plus `\nfor (let i = 0; i < 100; i++) await null` for `flags: [async]` files |
| `limits` | `{timeoutMs?, maxToolCalls?, maxOutputBytes?}`, only the ones set |
| `hooks` | names of the hooks configured on the runtime (`"tool.before"`, `"tool.after"`, `"extension.before"`, `"extension.after"`) |
| `extensions` | `[{name, globals: [globalName, ...], async: [globalName, ...]}]` in configuration order (`async` lists the globals for which `Extension.isAsync` holds); `null` for `via: "executeProgram"` |
| `tree` | the runtime's tool trie (below) |
| `events` | everything that crossed the host boundary, in `t` order (below) |
| `result` | the `CodeMode.Result` the execution produced (`{ok: true, value, warnings?, logs?, truncated?, toolCalls}` or `{ok: false, error: {kind, message, location?, suggestions?}, logs?, truncated?, toolCalls}`) |
| `interrupted` | `true` instead of `result` when the test interrupted the execution's fiber (2 records) |
| `defect` / `unfinished` | instead of `result`: the execution died (never observed) / never finished before the test file ended (never observed) |

The test262 harness (`assert`, `assert.sameValue`, `assert.notSameValue`, `assert.throws`, `assert.compareArray`,
`compareArray`, `compareArray.format`, `Test262Error`, `Test262Error.thrower`, `$DONE`, `$DONOTEVALUATE`) is
provided as native host globals by upstream `test/test262/run.ts` (port that file's semantics, including its failure
message formats) and is not an extension, so it produces no events. The test passes when: for `negative` frontmatter, `result.ok === false`
(and for parse-phase negatives `error.kind === "ParseError"`); otherwise `result.ok === true` and, for async files,
`$DONE` was called without an error. Parse the frontmatter from `code`. Skipped files
(`skipped.txt`) are not run.

### Tool trie (`tree`)

`ToolRuntime.prepare(tools)` → `toolTrie`, dotted names already split into segments:

```jsonc
{ "children": [[segment, node], ...],     // Map insertion order (object key order of the tools record)
  "namespace": { "description"? },        // present when a Namespace.make sits at this node
  "tool": { ... } }                       // present when a tool sits at this node (last one wins)
```

A tool:

| Field | Description |
| --- | --- |
| `description` | tool description |
| `input` | schema record of `tool.input` |
| `output` | schema record of `tool.output`, or `null` when the tool has no output schema (result type `void`) |
| `signature` | exactly what `describeTool` renders for catalog/search: `toolExpression(path)` + `"()"` or `"(" + inputTypeScript(tool, true) + ")"` + `": Promise<" + outputTypeScript(tool, true) + ">"` |
| `inputTs` / `outputTs` | `inputTypeScript(tool, false)` / `outputTypeScript(tool, false)` (compact) |
| `emptyInput` | `isEmptyInput(tool)` |
| `inputProperties` | `inputProperties(tool)`: `[{name, description, required}]` (`description` may be `{"$undefined": true}`) |

Any of the rendered fields may be `{"$throws": message}`.

Schema record:

- raw JSON Schema: `{"kind": "json", "schema": <the object as given>}`
- Effect Schema: `{"kind": "effect", "document": ..., "emptyDocument": ...}` where `document` is
  `Schema.toJsonSchemaDocument(input, {onExcessProperty: "error"})` (`{dialect, schema, definitions}`) for inputs and
  `Schema.toJsonSchemaDocument(Schema.toType(output), {onExcessProperty: "error"})` for outputs, or `null` if that
  throws; `emptyDocument` (inputs only) is `Schema.toJsonSchemaDocument(input, {referencePolicy: () => undefined}).schema`,
  the document `isEmptyInput` inspects. In `schema-calls.jsonl`, a schema passed on its own (`toTypeScript`) also
  carries `typeDocument` (the `toType` rendering). The Rust side cannot run Effect decoders, so decoding behaviour
  of Effect schemas is visible only through `decode` events and the documents.

### Events

Every event has `type` and `t`, a counter shared by all events of the execution (monotonic in the order things
happened). Tool calls are identified by `id` (0-based per execution, in call order), extension calls by `xid`.

**`call`** — the program called `tools.<path>(...)`, `tools.search(...)` or `search(...)`, after its arguments
crossed to JSON:
`{"type": "call", "name", "path"?, "args": [...], "id"}`. `name` is the canonical dotted path (segments of `path`
split on `.` and joined) or `"search"` (bare `search(...)`, or `tools.search(...)` when no tool owns `search`).
`path` is the raw property path the program used (absent for bare `search`). `args` is the JSON argument list
(`undefined` encoded), i.e. what the runtime received.

**`resolve`** — `{"type": "resolve", "id", "ok": false, "error"}`: the path named no callable tool
(`UnknownTool` message, including any "Did you mean" hint). Only failures are recorded.

**`decode`** — input validation:
`{"type": "decode", "id", "ok": true, "input"}` with the decoded input handed to the tool, or
`{"type": "decode", "id", "ok": false, "error", "suggestions"?, "arity"?}` with the full `ToolRuntimeError`
message. `arity: true` marks the "expects at most one input object" error (more than one argument); zero arguments
are decoded as `{}`.

**`admit`** — the call-count limit: `{"type": "admit", "id", "ok": true, "count"}` (`count` = calls admitted so far,
including this one) or `{"type": "admit", "id", "ok": false, "error"}` (`ToolCallLimitExceeded` message). Only an
admitted call appears in `result.toolCalls`.

**`hook`** — a configured hook was invoked:
`{"type": "hook", "hook": "tool.before" | "tool.after" | "extension.before" | "extension.after", "id"? | "xid"?,
"call", "result"?, "outcome", "failed"?, "errorClass"?, "errorValue"?, "channel"?, "settledT"}`.
`call` is the invocation passed to the hook (`{name, input}` for tools, `{extension, name, args}` for extensions),
`result` (after-hooks only) is the `CallResult` (`{status: "success", value}`, `{status: "failure", error}`,
`{status: "interrupted"}`). `outcome` is how the hook's Effect ended: `"success"`, `"failure"` (then `failed` is the
error message, `errorClass` its constructor name, `errorValue` the encoded error, `channel` `"fail"` or `"die"`), or
`"interrupted"`; `settledT` is the `t` at which it ended. A failing `before` hook denies the call: the tool does not
run, the call's `settle` carries the hook's error.

**`settle`** — how the call ended, as the interpreter sees it:

| Field | Description |
| --- | --- |
| `id` | call id |
| `outcome` | `"success"`, `"failure"` (the tool or a before-hook failed), `"invalidOutput"` (`InvalidToolOutput`), `"rejected"` (any other `ToolRuntimeError`: unknown tool, invalid input, arity, call limit), `"interrupted"` |
| `value` | on success: the final JSON returned to the interpreter (after output decoding and the `JSON.stringify`/`JSON.parse` round trip; `{"$undefined": true}` when that round trip yields `undefined`) |
| `error` | otherwise: the error message. For a tool failure it is the `ToolError` message built by `Cause.prettyErrors` formatting; for runtime errors the full `ToolRuntimeError` message |
| `errorClass` | constructor name of the error (`"ToolError"`, `"ToolRuntimeError"`, or a hook's own error class) |
| `errorKind` | `ToolRuntimeError` only: `UnknownTool`, `InvalidToolInput`, `InvalidToolOutput`, `ToolCallLimitExceeded` |
| `errorValue` | for other error classes (e.g. a failing before-hook): the encoded error |
| `channel` | `"fail"` (typed failure) or `"die"` (defect, e.g. the arity/unknown-tool errors thrown synchronously) |
| `toolRan` | whether `tool.execute(input)` was started |
| `toolOutcome` | when it ran: how the tool's own Effect ended (`"success"`, `"failure"`, `"interrupted"`), before error mapping or output decoding |
| `raw` | when the tool succeeded: the value it returned, before output decoding — what a replay harness's fake tool should return |
| `rawError` | when the tool failed: `{class, channel, message, value}` of the tool's own error (before conversion to `ToolError`) — what a fake tool should fail with |
| `sync` | when it ran: `true` if the tool's Effect completed before any microtask could run after it started (see below) |
| `toolMs` | when it ran: wall-clock duration of the tool's Effect in whole ms; informational and machine dependent |
| `callSync` | the same test as `sync`, applied to the whole call (resolution, decode, admit, hooks, tool, output) |

`sync` approximation: when the tool's Effect starts, the recorder takes a "turn token" (an object that a
`queueMicrotask` callback queued at that moment invalidates). When the Effect exits it checks whether the token is
still current. Effect's fiber run loop executes synchronous work without yielding to the microtask queue, so `true`
means the tool completed within the same synchronous slice, i.e. before the calling fiber had to suspend
(`Effect.succeed`/`sync`/`fail`, pure computation). Anything that awaited a promise, timer or `Effect.sleep`/`yieldNow`
lets microtasks run and is `false`. Edge case: Effect's scheduler can also yield after a long run of synchronous
operations; such a tool would be reported `false` even though it never awaited (not observed in practice).

**`extension`** — the program called a host function from an extension:

| Field | Description |
| --- | --- |
| `xid` | extension call id (0-based per execution) |
| `label` | the program-facing name: the global's name, or `<global>.<path>` for a function found inside a value returned by an extension (e.g. `"fetch.json"`) |
| `global` | `true` for a configured global (runs inside extension hooks), `false` for a function inside a result (no hooks) |
| `extension`, `name` | globals only: extension name and global name |
| `async` | whether the called function is asynchronous (`Extension.isAsync`) |
| `args` | arguments after crossing to host values |
| `returned` | how the host function returned: `"value"`, `"throw"`, or `"promise"`. A call the `extension.before` hook denies never reaches the host; it is recorded with `"promise"` for an async global and `"throw"` for a sync one |
| `outcome` | `"success"` (then `value`, the host value before it crosses back into the program), `"failure"` (then `error`, `errorClass`, `errorValue`, `channel`), or `"interrupted"`; hooks included |
| `settledT` | `t` when it settled |

Arguments that cannot cross (functions, opaque values, cycles) throw in the program before any event is recorded.

## `catalog.jsonl`

`{file, test?, openapi?, tree, catalog: [{path, description, signature}]}` — `runtime.catalog` as returned
(sorted by path), with `tree` the same tool trie as above. Deduplicated on (file, tree, catalog); `test` is the first
test that read it.

## `schema-calls.jsonl`

`{file, test?, fn, args, result | throws}`: one direct call from a test into `src/tool-schema.ts`. `fn` is one of
`toTypeScript`, `jsonSchemaToTypeScript`, `inputProperties`, `inputTypeScript`, `isEmptyInput`, `outputTypeScript`,
`decodeInput`, `decodeOutput`. In `args`, a tool argument is `{"tool": {description, input, output}}` with schema
records as above, a schema argument is a schema record, and the remaining arguments (`pretty`, `decoded`, a value to
decode) are encoded values. `throws` is `{name, message}`.

## `make-errors.jsonl`

`{file, test?, tools, limits, extensions, error: {name, message}}`. `tools` is the configuration as given, with
tools shown as `{"$tool": {description}}` and namespaces as `{"$namespace": {description?, tools}}`.

## `openapi-fromspec.jsonl`

`{file, test?, spec, options, tree, skipped}` or `{..., throws}`. `options` holds everything but `spec` (`baseUrl`,
`headers`, `auth` functions as `{"$function": name}`). `tree` uses the trie format with only `description`, `input`
and `output` per tool (signatures are not rendered: one upstream test builds a reference diamond whose rendering is
exponential).

## `openapi-http.jsonl`

`{file, test?, operation: {operationId, method, path, summary, description}, planUrl, input, request, response,
outcome, value | error, errorClass?}`. `request` is `{method, url (with query), headers, body: {tag, contentType?,
text?}}` as handed to the `HttpClient` after auth was applied, or `null` if the call failed before sending.
`response` is `{status, headers, body?}` (`body` = response text when it was read), or `null`. `outcome` is the
tool's own result (`value` = parsed body; `error` = `ToolError` message).

## `tests.jsonl`

`{file, test, counts}` for every test body that ran (pass 1 and pass 2), where `counts` maps `execute`, `catalog`,
`schemaCall`, `makeError`, `fromSpec`, `openapiInvoke` to how many were started inside it. A test with no `execute`
count bypasses `CodeMode.make().execute`.

## `search-signature.json`

`{"searchSignature": string, "toolExpression": [[path, expression], ...]}` — `CodeMode.searchSignature` and
`toolExpression(path)` for curated edge cases followed by every tool path seen in the fixtures.

## Nondeterminism

`toolMs` varies between runs. Programs racing a `timeoutMs` against slow work can land on either side under heavy
machine load; `built-ins/RegExp/character-class-escape-non-whitespace.js` hits its 5 s `timeoutMs` upstream too.

## Sampling note (Rust port)

The checked-in `test262/` shards keep a deterministic quarter of the recorded test262 executions (records whose
`sha1(test)` is divisible by 4: 3 340 of 13 330) to keep the repository small; `record.sh` regenerates the full set.

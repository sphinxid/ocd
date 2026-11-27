# oc-codemode

Rust port of opencode's `@opencode/codemode` (upstream `packages/codemode`, plus the model-facing glue from
`packages/core/src/codemode`): a confined, tree-walking interpreter for a JavaScript subset that lets a model script
host tools from a single `execute` tool call.

Programs can only use the supported JavaScript built-ins (with Node's `Buffer` since 2.3.1), the `tools` tree, the
`search(...)` discovery function (synchronous, and its result can also be awaited or chained like a Promise), and
globals the host adds through extensions (core adds `fetch`, and with the opt-in `experimental.full_codemode` also
`path`, see The `path` global). `import`, `import()` and
`require` load those globals by Node module name (see Modules). Classes, getters/setters, BigInt, arbitrary symbols,
timers, exports, `eval`/`Function`, and TypeScript syntax are rejected with the same diagnostics as upstream. The exact support matrix is upstream's `interpreter-support.md`; behaviour, diagnostics, and model-visible
strings match upstream byte for byte (verified by replaying upstream's recorded test executions, see Tests).

## Architecture

| Module | Role (upstream source) |
| --- | --- |
| `parser` | Port of acorn 8.15 (`ecmaVersion: "latest"`, script, top-level `return`/`await`, `allowImportExportEverywhere`), including its regex validator; exact acorn error messages and UTF-16 positions |
| `ast` | ESTree-shaped AST |
| `value`, `object`, `number` | Values (UTF-16 strings, f64, two confined symbols), objects with prototype chains and JS property order, ECMAScript number formatting/parsing (`objects.ts`) |
| `interp::eval` | Evaluator (`interpreter.ts`): statements, expressions, closures, destructuring, generators, async functions |
| `interp::runner` | The job queue (`runner.ts`): promise jobs in V8 order, promise states and reactions, `await`, host work settling through an inbox, blocking host calls, stall detection, unhandled-rejection warnings |
| `interp::promises`, `generators`, `iteration`, `coerce`, `callback`, `errors`, `globals`, `extensions`, `modules` | Promise built-ins and combinators, generator state machines, iterator cursors and CreateAsyncFromSyncIterator, ToPrimitive, callback admission, diagnostics, global table, host-value boundary, `import`/`require` module resolution (`modules.ts`) |
| `stdlib::*` | Standard library (`stdlib/*.ts`): Object, Array, String, RegExp (`regress`), Number, Math, Date, JSON, Map/Set/Weak*, Iterator, URL/URLSearchParams, Headers, Uint8Array, Node's Buffer (2.3.1, `stdlib/buffer.rs`; see Buffer), TextEncoder/Decoder (`decode(bytes, { stream: true })` keeps a split character for the next chunk and strips only a stream's first BOM, 2.3.0), atob/btoa, crypto, structuredClone, console |
| `data` | JSON boundary (`data.ts`): `toJson`/`toBoundary`/`fromJson`, JS-compatible JSON text |
| `tool_runtime`, `tool_schema` | Tool trie, `search`, admission/limits/hooks (`tool-runtime.ts`); JSON Schema → TypeScript signatures (`tool-schema.ts`) |
| `codemode` | Public runtime API, limits, output bounding (`codemode.ts`, `interpreter/execute.ts`) |
| `catalog`, `execute_tool`, `web` | Core glue: catalog summarization and instruction texts (`core/src/codemode/{catalog,instructions}.ts`), the `execute` tool's description/schema/result formatting/progress rows (`codemode/tool.ts`), and the `fetch` extension (`codemode/fetch.ts`) |
| `openapi` | OpenAPI 3.x → tools adapter (`openapi/*.ts`; unused by core upstream) |
| `path`, `glob` | The `full_codemode` `path` global (core's `codemode/path.ts`): Node's `path` module as Bun 1.4 gives it, both flavours, and Bun's glob matching for `matchesGlob` |

Each execution runs on a dedicated thread (1 GiB stack, so deep recursion up to the 10 000-call limit is safe) with
its own job queue. Program code runs only inside a job, each to completion: the program body is the first job,
promise reactions and resumed awaits are queued in V8's order, and an async body (async function, async generator,
the program) is a Rust future that parks at an `await` and is polled again by the job that resumes it. Host work
(tool calls, async extension globals such as `fetch`) runs as host futures polled on that thread inside the caller's
tokio runtime context; they wake the interpreter through a thread-safe queue and settle their promise in a later
turn, after the jobs already queued. `search(...)` and synchronous extension globals hold the current job, also while
host work they block on runs (`HostCall::Block`). A program
stuck at an `await` with nothing queued and no host work running fails at once, pointing at that `await`. When the
program returns, host work still running is dropped (tool `after` hooks see `Interrupted`), and all objects of the
run are torn down to release reference cycles.

## Public API

### Runtime

```rust
use std::sync::Arc;
use oc_codemode::codemode::{CodeMode, ExecutionLimits, ExecResult};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};

let runtime = CodeMode::new(
    vec![
        ToolEntry::Namespace { path: "orders".into(), description: Some("Purchases and shipments".into()) },
        ToolEntry::Tool(
            ToolDef::new("orders.lookup", "Look up an order by ID", serde_json::json!({
                "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"]
            }))
            .with_output(serde_json::json!({ "type": "object", "properties": { "status": { "type": "string" } } })),
        ),
    ],
    Arc::new(my_host),                          // impl oc_codemode::host::Host
    vec![oc_codemode::web::extension(Arc::new(oc_codemode::web::ReqwestClient::new()))], // `fetch`
    ExecutionLimits::default(),                 // timeout_ms / max_tool_calls / max_output_bytes: None = unlimited
)?;

let catalog = runtime.catalog();                // Vec<ToolDescription { path, description, signature }>, sorted by path
let result: ExecResult = runtime.execute("return await tools.orders.lookup({ id: 'o1' })").await;
let json = result.to_json();                    // upstream CodeMode.Result shape and key order
```

- `CodeMode::new` validates limits (`timeoutMs must be a safe integer greater than or equal to 1.` …), rejects empty
  tool-name segments, and rejects extension globals that shadow built-ins (`Extension "x" global "y" is already
  defined.`) — returned as `CodeModeError::{Range, Type}`.
- `CodeMode::execute(&self, code) -> ExecResult` is a `Send` future; dropping it interrupts the program.
  `execute_blocking` runs it on a private current-thread runtime.
- Per-run extensions (2.3.1, #64482, upstream `execute(code, bindings, { extensions })`):
  `runtime.select_extensions(&[ext])` is the same runtime (tools, limits and bindings shared) whose runs get only those
  of its extensions, in the runtime's order (`&[]` for none); each must be one the runtime was made with, the same
  `Arc`, else `CodeModeError::Type("Extension \"<name>\" isn't one this runtime was made with.")`.
  `select_extensions_named(&["fetch"])` picks by name; `execute_with(code, &RunOptions { extensions: Some(..) })` is
  `select_extensions(..)?.execute(code)`. Core makes one runtime with every extension and picks `fetch` alone, or
  `fetch` and `path` with `full_codemode`.
- `ExecResult::{Success { value, warnings, logs, truncated, tool_calls }, Failure { error, logs, truncated,
  tool_calls }}`; `Diagnostic { kind: DiagnosticKind, message, location: Option<Location { line, column }>,
  suggestions }` with the eleven upstream kinds (`ParseError`, `UnsupportedSyntax`, `UnknownTool`,
  `InvalidToolInput`, `InvalidToolOutput`, `InvalidDataValue`, `ToolCallLimitExceeded`, `TimeoutExceeded`,
  `ToolFailure`, `ExecutionFailure`, `Truncated`).
- Tool paths: dots create namespaces; the last tool for a path wins; a path may be both a tool and a namespace.
  `ToolDef.input`/`output` are `tool_schema::ToolSchema::Json` (render-only JSON Schema) or `ToolSchema::Document`
  (a pre-built schema document with top-level definitions). `output: None` renders `Promise<void>` and the program
  receives `undefined`.
- `search("text")` or `search({ query?, namespace?, limit?, offset? })` (a plain string is the query since 2.3.1,
  #64347, also as `tools.search("text")`; bad input suggests `Pass search a query string or an object, such as
  search("issues") or search({ namespace: "github" }).`) ranks tools as 2.3.0 does (#62984): field-weighted BM25 (k1 1.2,
  b 0.75; the tool's name 3, its namespace 2, description 1, namespace descriptions 0.4, parameter names 0.3) over
  whole words (split at case changes and non-alphanumerics, camelCase words also whole, plural endings stemmed), a
  4+-letter query word also matching longer words it starts with at 0.3, a boost for short names, ties by path. The
  same ranking picks an unknown tool's "Did you mean …?", and calling a namespace names up to three of its tools
  ranked by the namespace's last segment (`Tool 'x' is not callable, it is a namespace. Did you mean tools.x.a,
  tools.x.b, or tools.x.c?`, suggestion `N more under 'x'. Use search({ namespace: "x" }) to list them.`, #63697).
- Helpers: `tool_runtime::tool_expression(path)` (`tools.a["b-c"]`), `tool_runtime::SEARCH_SIGNATURE`,
  `tool_runtime::signature_of(&ToolDef)`, `tool_schema::{json_schema_to_typescript, schema_to_typescript,
  input_properties, is_empty_input}`.

### Host interface (`host`)

```rust
pub trait Host: Send + Sync + 'static {
    /// One admitted tool call; `path` is the canonical dotted path ("ns.tool"), `input` the decoded input
    /// (None = `undefined`). Ok(None) = `undefined`.
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>>;
    /// Validation before admission; Err(msg) → InvalidToolInput "Invalid input for tool '<path>': <msg>".
    fn decode_input(&self, path: &str, input: Option<Json>) -> Result<Option<Json>, String> { Ok(input) }
    fn tool_before(&self, call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>>;   // deny = Err
    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {}
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>>;
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {}
}
```

`ToolCallError::Failure(msg)` → `ToolFailure`; `ToolCallError::InvalidOutput(msg)` → `InvalidToolOutput: Invalid
output from tool '<name>': <msg>`. Hooks also run around `search` (as tool `search`), which counts toward
`max_tool_calls`. `ToolInvocation { id, name, input }`, `CallResult::{Success, Failure, Interrupted}`.
`NoTools` is a host without tools.

Extensions (`host::Extension`) expose globals: `Extension::globals()` gives functions (`SimpleExtension`), and
`Extension::values()` (default: those functions) any global value (`GlobalsExtension { name, globals: Vec<(String,
HostValue)> }`). A global is a function or an object (else `CodeMode::new` fails with `Extension "<name>" global
"<global>" must be a function or an object.`, upstream `Extension.make`); since 2.3.1 (#61151, #64472) an object
global such as `fs` or `path` is a `HostValue::Object` whose functions, at any depth of its plain objects, are called
like global functions and named by their path (`fs.promises.readFile`) in diagnostics and in
`ExtensionInvocation::name`; the rest of it is copied in as data, once per run, into a plain object the program may
change. Host functions are built with `host::HostFn::new(name, length, |args| ...)`, optionally `.asynchronous()`
(upstream `Extension.async`) and `.constructible()` (upstream `Extension.constructible`: `new f(...)` calls it as
`f(...)` does and gives what it returns; otherwise `TypeError: new f(...) is not supported; call f(...) without new
instead.`), then `.into_value()`/`.into_arc()`; `host_fn`/`host_fn_async`/`FnHostFunction` remain. The
`extension_before` hook runs before the host function, so a denied call never reaches the host: an asynchronous host
function (`HostFunction::asynchronous`) always returns a promise, which a denial rejects, while a synchronous one
throws. A host function answers `HostCall::Ready` (a value now), `HostCall::Pending` (a host promise: the program gets a
promise) or, since 2.3.1 (#64363), `HostCall::Block` (host work a synchronous function's caller waits for, as for a
synchronous global returning an Effect upstream: the program's job stops until it ends and then gets its value or
catches what it failed with, nothing else in the program running meanwhile; from an asynchronous function it is a
promise). Values cross as `HostValue`
copies (plain data, Date, RegExp, URL, URLSearchParams, Headers, Map, Set, bytes, Node `Buffer`s, errors with
name/message/cause/own fields, and nested host functions that become callable program functions). A function inside a
value a host function returned, passed back to any extension function (on its own or inside an argument), arrives as
the same `HostValue::Function` (the same `Arc`, so the host can tell with `Arc::ptr_eq` which it was; 2.3.1 #64472,
core's `takesFunctions`); a function among the globals or inside a global object never does, and is left out like the
program's own functions, and tools never receive functions. Arguments cross
as upstream 2.3.1's core translates them for every host function, tools included (#64487, #64513): a function
anywhere is left out (an object's key skipped, an array item `null`, a Map or Set entry or a whole argument
`undefined`), a promise is replaced by its value (a pending one is awaited first and the call made again: an
asynchronous call's promise follows that call's, a synchronous call waits where it is, as `await` would; a rejection
fails the call with its reason), and a program's own `toJSON` is never run. An iterator, a generator, a symbol or
`tools` itself fails with `Argument <n> to <label> contains <what>, which cannot be passed to the host.` (rejecting an
asynchronous call's promise, thrown by a synchronous one), and a value deeper than 32 levels with `... exceeds the
maximum value depth of 32.` A tool then receives that copy as JSON (`tool_runtime::host_to_json`, upstream's
`toJson`): Dates as ISO text (`null` when invalid), URLs as `href`, URLSearchParams as their query, Headers as an
object, Sets as arrays, errors as `{name, message, ...own fields}`, Maps and RegExps as `{}`, and a Uint8Array or
Buffer refused with `InvalidDataValue` `A Uint8Array cannot cross to a tool; pass text instead, ...`. `HostValue::Live` is a host object with
accessors read from the host on every access (non-enumerable) and `Symbol.asyncIterator` methods (`HostKey`,
`HostProperty`); `fetch` builds its streaming responses from it. A host function returns
`HostCall::Ready`, `HostCall::Pending` or `HostCall::Block` (above).

HTTP: `host::{HttpClient, HttpRequest, HttpResponse}`; `HttpClient::send_streaming` returns an
`HttpStreamingResponse` once the head arrives, its body a `BodyStream` of chunks (the default buffers `send`);
`web::ReqwestClient` is the default client. `web::extension(client)` is the `fetch` extension exposing `fetch(url, {
method?, headers?, body? })` with a 30 s timeout (body included) and, since 2.3.0 (#63436), a streaming response like
Bun's: `{ url, status, statusText, ok, headers, redirected, body, bodyUsed, text(), json(), bytes(), arrayBuffer() }`.
`headers` is a real `Headers`; `body` is `null` for HEAD, 204, 205 and 304, else a stream with `locked`, `cancel()`,
`getReader()` (`{ read, cancel, releaseLock }`; `getReader({ mode })` is `TypeError: Only the default stream reader is
supported.`) and `for await` iteration; the whole-body readers consume it once (`TypeError: Body already used`, as
are a locked or read stream's), `arrayBuffer()` gives a `Uint8Array`, and `bodyUsed` follows Bun (set by a reader too).
Bun's lock and detached-reader errors and its JSON errors for an empty body are reproduced; a timeout is
`Error: The operation timed out.` (Bun's `TimeoutError` crossing). A body still open when the program ends is
cancelled with the run. `web::extension_with(client, Some(authorize))` checks each call after the `file:` refusal
and before anything is sent (core asserts the `webfetch` permission, `codemode/fetch.ts`); an `Err` rejects the
program's promise. Since 2.2.7 the URL loses its `#fragment` before any check, a URL with credentials is a
`TypeError` (`fetch cannot be used with credentials in the URL.`), and `web::extension_checked(client, authorize,
check_redirect)` follows redirects itself (give it `ReqwestClient::manual()`): up to 20 hops under one 30 s deadline,
refusing non-http(s) targets and targets with credentials, discarding each redirect's body, checking each target
(without its fragment) with `check_redirect(next, args, url)`, and rewriting the request as a browser does (303, or 301/302 after a POST, become a
GET without body or content headers; another origin loses `authorization`, `cookie` and `proxy-authorization`). The
response's `url` is the last target as written and `redirected` whether any hop was followed.
`web::extension_api(client, authorize, check_redirect, refusal)` adds 2.4.1's `opencode:` URLs (#65725): only
`opencode:///api/...` passes (`TypeError: Invalid opencode URI "…": use opencode:///api/...`), with no permission
check, and is then refused with `refusal` (core passes `OPENCODE_DISABLED`, a release build's text) or
`OPENCODE_UNAVAILABLE` (no OpenCode API, as for an automation's program); ocd has no in-process API to send it to. `ProgressRows::interrupt_running` settles the rows of calls still running as errors when an
execution is interrupted.

### The `execute` tool and catalog instructions

- `execute_tool::{NAME, DESCRIPTION, input_schema()}` — the tool definition (verbatim upstream description).
- `execute_tool::format_result(&ExecResult)` — the text output (value as text or compact JSON since 2.3.0, #63258, or
  error message plus suggestions, then `Warnings:` and `Logs:` sections; since 2.3.1, #64344, a `null` value is left
  out when warnings or logs follow, so a program that only logs gives `Logs:\n...`); `execute_tool::format_execute_output(text)`
  re-indents it for display as clients do (`@opencode/util/execute-output`: the JSON result through `JSON.parse` and
  `JSON.stringify(_, null, 2)`, the sections apart; the TUI's execute dialog uses it); `execute_tool::output(...)` builds the
  `{ output, toolCalls, error?, files }` structure; `ProgressRows` tracks `{ tool, status, input? }` progress rows
  from the host hooks (fetch rows show `{ method, url }`); `display_input`, `fetch_display`, `output_file`.
- `catalog::inventory(&catalog, &namespaces, &pinned)` builds the catalog tree; `catalog::summarize(&inventory,
  None)` applies the 2 000-token inline budget (inline descriptions cut at 2 000 UTF-16 units since 2.4.1, #65493; 500 in 2.3.1; a pinned tool shows its whole description, each further line as a `    // ` comment) (chars/4 rounded; every namespace line shown; pinned listings always
  shown; remaining listings round-robin per namespace in path order, a namespace stopping at its first listing that
  does not fit). The stored summary is `{total, namespaces: [{name, description?, tools: [every path], entries}]}`
  (2.2.7); a 2.2.5 summary does not decode, so core restates the whole catalog. `catalog::render(&summary)` produces
  the `core/codemode` instruction text; `catalog::update(&previous, &current, &lookup(&inventory))` the change delta
  (#62176): new namespaces and tools (new listings share a 1 000-token budget while the catalog is partial, the rest
  counted), the `search` note when the catalog becomes partial, changed listings the model saw, and removed tools and
  namespaces — or the whole catalog when that is shorter, or `None` when nothing the model was told changed.
  `catalog::{EMPTY_CATALOG, REMOVED_CATALOG}`.
- `experimental.codemode_vm` (#62358) was a flag for upstream's bytecode VM. Since 2.3.0 upstream always runs the VM
  and the key is deprecated and ignored; ocd accepts it and runs this interpreter, which is tested against the VM.

### OpenAPI (`openapi`)

`openapi::from_spec(options)` turns an OpenAPI 3.x document into `ToolEntry`s plus a `skipped` list;
`OpenApiHost` executes the generated tools over an `HttpClient` with host-resolved auth. See the module docs.

## Tests

`cargo test -p oc-codemode` (all green):

- `tests/upstream_replay.rs` — replays every program execution recorded from upstream's test suite and requires an
  identical `Result`. Three corpora, all generated by `tests/recorder/record.sh`, which instruments upstream (format
  in `tests/fixtures/upstream/README.md`):
  - `tests/fixtures/upstream-2.3.1/`: the 2.3.1 suite, after the language/Code Mode split. 1 873 of 1 875
    recordings match; the 2 others, in `tests/fixtures/replay-skips-2.3.1.txt`, are the 2 permanent divergences (a
    timed-out tool's uninterruptible cleanup). Its tool calls also check each argument as it reached the tool (the
    recording's `json`), and `tests/upstream_2_3_1.rs` covers what the suite does not.
  - `tests/fixtures/upstream/`: the 2.2.2 suite on the legacy interpreter (with 2.2.7's three changed results).
    1 771 of 1 802 recordings match; the 31 others are in `tests/fixtures/replay-skips.txt`: 2 permanent divergences,
    2 results 2.3.0 changed (BM25 `search` no longer finds `get_timezones` for "zones", and calling a namespace
    names its tools) and 27 that 2.3.1 changed (reworded diagnostics and the `search` suggestion), each naming the
    record of the later corpus that carries the new result.
  - `tests/fixtures/upstream-2.3.0/`: the 2.3.0 suite on the bytecode VM, the only engine from 2.3.0. 1 815 of
    1 843 recordings match; the 28 others, in `tests/fixtures/replay-skips-2.3.0.txt`, are the same 2 permanent
    divergences and 26 results 2.3.1 changed (a record listed with a batch tag such as `P5:` must still differ, so a
    batch that ports its feature deletes the line). No recorded program differs because of the engine: on the 1 859 programs both
    suites run, upstream's own results are identical except the namespace-call message, and so are the 3 340
    sampled test262 files. Replayed once against the full 2.3.0 test262 recording (13 330 files), ocd is identical
    except 9 files that write array indices near 2^32, beyond this crate's dense 10 000 000-element arrays (the
    sample's one documented divergence and 8 outside the sample, where 2.2 gave the same results).

  The recorder kit (`tests/recorder/`) now targets 2.3.1's `codemode/{codemode,tool-runtime}.ts`,
  `language/runtime.ts` and `language/runtime/{execute,extensions}.ts` (`tests/fixtures/upstream-2.3.1/README.md`).
  `OC_CODEMODE_CORPUS=2.3.0` (or `2.3.1`) points the tests that read one corpus (`fixtures()`, e.g.
  `tests/openapi.rs`) at that recording.
- `tests/upstream_2_4_1.rs` — programs run through upstream 2.4.1's Code Mode, which runs the language as the
  `psychoscript` 0.2.1 package (`tests/fixtures/cases-2.4.1.json`, regenerated by `tests/recorder/cases-2.4.1.ts`, run
  with `TZ=UTC`), in the runtime of `upstream_2_3_1.rs` plus an `open` global that throws a host Error: the import
  errors, Buffer derivation and base64 options, error stacks, and the 2.3.1 cases whose result 2.4.1 changed (those
  carry a `gap` note in `cases-2.3.1.json`). Older corpora are compared with 2.4.1's rewordings applied
  (`common::reworded_2_4_1`: the supported-syntax summary, `Symbol(psychoscript.iterator)`, the import errors);
  records whose result 2.4.1 changed otherwise are in the skip lists with the case that carries the new result.
- `tests/upstream_2_3_1.rs` — 1 188 programs run through upstream 2.3.1 (`tests/fixtures/cases-2.3.1.json`,
  regenerated by `tests/recorder/cases-2.3.1.ts`) with an echo tool and echo extensions: arguments crossing to tools
  and extensions (left-out functions, awaited promises, `toJSON` not run, the refused values and their messages,
  ordering against program jobs), `search("text")`, the reworded diagnostics, and about 800 `Buffer` programs
  (every encoding, the statics, views, `toString` ranges, `indexOf`/`write`/`fill`/`copy`/`compare` with their
  argument checks, every number read and write with its range errors, swaps, `console.log`, JSON, the `buffer`
  module, crossing to and from the host). All identical.
- `tests/upstream_2_2.rs` — the cases upstream added in 2.2: promise jobs in V8 order (29 cases), stalled awaits,
  tool results settling like I/O, located parse errors, the nesting-depth failure, timeouts inside built-in work,
  promise-like `search` results, `for...in` over inherited keys, generator receiver checks, and extension `before`
  hooks that run before the host function.
- `tests/upstream_2_1.rs` — the cases upstream added up to 2.1.4: jobs run to completion, the `Uint8Array.from`
  mapper, non-coercing byte searches, `flatMap` holes, the host's `Number#toString` radix check, `fetch` refusing
  `file:` URLs, the fetch display method fallback, and recursion depth as 2.1.4 reports it.
- `tests/extensions_replay.rs` (44 recordings from 2.2, 63 from 2.3.0 and 77 from 2.3.1; the `extension-skips*.txt`
  lists hold the records a replay cannot rebuild and those 2.3.1 changed: 5, 14 and 6) and `tests/hooks.rs` (the
  recordings with hooks of `codemode.test.ts` and `extensions.test.ts`) — extension host functions and call hooks,
  including hook ordering and denials, object globals (a global the recording only calls by longer paths, `fs.readSync`,
  is an object), constructible globals (one the program calls with `new`), and host work a synchronous global blocks on
  (recorded as `"returned": "effect"` since this kit's 2.3.1 re-recording of `extensions.test.ts`). The 2.3.1 test262
  run is byte-identical to 2.3.0's, which `tests/test262_sample.rs` replays.
- `tests/extensions_2_3_1.rs` — the 2.3.1 extension API as the host sees it (upstream `extensions.test.ts`): a function
  from a result passed back as the same `Arc` and never to a tool, globals and program functions left out, constructible
  and blocking globals, object globals through `import`/`require` with the hooks named by path, per-run selection.
- `tests/path_module.rs` — `path::Module` against Bun 1.4.4 on 8 100 generated calls of every function of both
  flavours (`tests/fixtures/path/path-calls.json`, `tests/recorder/path-calls.ts`) and `matchesGlob` on 5 220
  generated patterns (`glob-calls.json`, `glob-calls.ts`).
- `tests/path_global.rs` — 106 programs using the `path` global through upstream 2.3.1's Code Mode with core's
  `path.ts`, on a POSIX host and as on a Windows host (`tests/fixtures/path/path-2.3.1.json`,
  `tests/recorder/path-cases.ts`), and core's own `tool-execute.test.ts` "path" tests.
- `tests/test262_sample.rs` — a deterministic quarter (3 340 files) of upstream's test262 conformance runs with a port
  of its harness, recorded from 2.2.2 and from 2.3.0 (whose harness defines `assert` in a one-line program prelude
  over the host's `$assert`); all identical except 1 documented in `tests/fixtures/test262-skips.txt` (and
  `test262-skips-2.3.0.txt`).
- `tests/parser_acorn.rs` — parser vs acorn 8.15 (with `allowImportExportEverywhere`) on 2 243 snippets and all
  15 008 vendored test262 files (AST and exact errors; regenerate with `node tests/fixtures/parser/gen.js`).
- `tests/upstream_2_3_0.rs` — where 2.3.0's VM differs from the 2.2 interpreter beyond upstream's own tests
  (`tests/fixtures/vm-2.3.0.json`, 80 programs run through upstream 2.3.0 by `tests/recorder/vm-cases.ts`). The
  VM's standard library is partly program code, so: built-ins that walk an argument with `for...of` (`new
  Map/Set/WeakMap/WeakSet`, `Object.groupBy`/`Map.groupBy`, `Object.fromEntries`, `new AggregateError`,
  `Math.sumPrecise`) fail with `InvalidDataValue` `TypeError: for...of requires an iterable value, received …`; a sort
  comparator's result is read as unary `+` reads it (`valueOf`/`toString` run and may throw, a promise is `Unary
  operators require data values.`); `String.replace` callback results and `Headers`/`URLSearchParams` pair items
  are converted with `String(…)` (an object's own `toString` runs); a Set-like operand with a negative size is a
  `RangeError`, a `keys()` result that is not an object is `Set.<method> expects 'keys' to return an iterator
  object.`; `GetIteratorDirect` on a primitive is `An iterator must be an object.`; well-known symbols print as
  `Symbol(codemode.iterator)`/`Symbol(codemode.async-iterator)` (`Symbol(psychoscript.iterator)` since 2.4.1). Not followed (9 cases noted `gap`): the VM reads an
  iterator's `next` without checking it and fails only when calling it, with a `<call> is not a function.` naming the
  program call that made the iterator.
- `tests/upstream_2_2_7.rs` — the 2.2.6–2.2.7 module support: 80 programs (`import`, `import()`, `require`, aliases,
  caching, located failures, exports, `import.meta`, extension globals as modules) run through upstream 2.2.7's Code
  Mode (`tests/fixtures/modules-2.2.7.json`), upstream's new `errors.test.ts`/`extensions.test.ts` cases, and
  `scanTools` on module code. The two `errors.test.ts` recordings of the replay corpus whose behaviour 2.2.7 changed
  (`import x from "y"`, `export const x = 1`) carry 2.2.7's results; the two nested-import programs of
  `modules-2.2.7.json` carry 2.3.0's.
- `tests/signatures.rs` — every recorded tool signature, catalog read, schema-rendering call, `toolExpression`, and
  `searchSignature` against upstream (both corpora; 2.3.0's freeform tool signature waits for batch P1).
- `tests/search_ranking.rs` — `search` over a generated catalog of 400 tools (camelCase, kebab and snake names,
  plurals, nested namespaces with descriptions, parameter names): 272 programs (queries, namespace filters, paging,
  exact paths, unknown tools and namespace calls) with the results upstream 2.3.0 gives
  (`tests/fixtures/search-2.3.0.json`, regenerated by `tests/recorder/search-cases.ts`).
- `tests/catalog.rs` — catalog summarization, instruction render and 240 update texts against goldens produced by
  upstream core 2.2.7 (`tests/recorder/gen-catalog-golden.ts`).
- `tests/stdlib_*.rs` — per-module parity cases whose expectations were produced by upstream (array, collections,
  iterator, string/regexp, number/math, date incl. other time zones, url/headers/bytes/web); all but the date cases
  were re-checked against 2.2.2, and all of them against 2.3.0, with `tests/recorder/stdlib-check.ts`
  (`STDLIB_DUMP=<file>` dumps them). The 11 whose result 2.3.0's VM changed carry 2.3.0's result; 2 more keep 2.2's
  (the `gap` of `tests/upstream_2_3_0.rs`).
- `tests/openapi.rs` — every recorded `OpenAPI.fromSpec` call and HTTP round trip, plus 49 extra upstream-generated
  cases.
- `tests/fetch_stream.rs` — 54 programs reading streaming responses from a local server (readers, `for await`
  with a streaming `TextDecoder`, one-shot bodies, null bodies, lock and reader errors, `Headers`, the response as
  JSON, a redirect, arguments that are promises or hold functions, programs that only log) with the `execute`
  outputs upstream core 2.3.1 gave against the same routes (`tests/fixtures/fetch-2.3.1.json`,
  `tests/recorder/fetch-cases.ts`), plus the bodies the server sees cancelled
  (left open at the program's end, a cancelled reader, a `break`, a redirect's body) and a pending request aborted
  when `execute` is interrupted.
- `tests/fetch.rs` (including the 2.2.7 fragment, credential and redirect rules), `tests/teardown.rs`, and unit
  tests in `number`, `tool_schema`, `catalog`, `execute_tool` (including upstream's `execute-output.test.ts`).

## Modules (upstream 2.2.6+, #61151)

Programs parse with acorn's `allowImportExportEverywhere` (still sloppy-mode scripts). A module is a global, by its Node
module name: `node:` is stripped, a global of that name is used (built-ins, bindings and extension globals), else the
aliases `fs/promises` (`fs.promises`), `url` (`{URL, URLSearchParams}`), `util` (`{TextEncoder, TextDecoder}`) and
`buffer` (`{Buffer, atob, btoa}`, only the globals that exist; `Buffer` since 2.3.1). Anything else fails with one of
2.4.1's three texts (#64587): ``Error: Cannot import "<name>": local files can't be imported.`` (a name starting with
`.`, `/` or `file:`), ``…: this runtime doesn't provide it.`` (`node:*`, `bun`, `bun:*`, or a Node core module by its
first segment, `interp::modules::is_runtime_module`), else ``…: packages can't be installed here.``

- Top-level import declarations bind immutably before any code runs (before hoisting): a default import binds the
  module, `* as ns` a null-prototype namespace of the module's own enumerable properties plus `default`, a named import
  one of the namespace's properties or `SyntaxError: "<spec>" has no export named "<name>".` at the binding. An import
  nested in the program's own blocks (`if`, loops, labels, `try`, `switch`) does nothing: it binds and loads nothing,
  as on 2.3.0's VM. One reached inside a function fails with `SyntaxError: An import declaration may only appear at
  the top level of the program.`
- `import(x)` (options evaluated and ignored) returns a promise of the namespace, rejected when the module fails to load.
- `require(name)` (global, after `search`) returns the module; a non-string fails with `TypeError: require expects a
  module name string.`
- Modules and namespaces are cached per execution, so repeated imports see the same object.
- A top-level `export` is a `ParseError`: `SyntaxError: Exports aren't available. Return the result instead.`; a nested
  one is `UnsupportedSyntax` when reached. `import.meta` parses and is `UnsupportedSyntax` (`MetaProperty`).
- Object-valued extension globals (#61151, e.g. an `fs` object whose functions fire the hooks) load like any other
  global, so `import { join } from "node:path"` works with the `path` global.

## Buffer (upstream 2.3.1, #64365)

`Buffer` is a global wherever the host has one, as on Bun, so in every run: a Uint8Array (`instanceof Uint8Array`,
`Object.prototype.toString` says `[object Uint8Array]`) whose prototype `Buffer.prototype` inherits
`Uint8Array.prototype`. `Buffer.from` (strings in `utf8`/`utf-8`, `hex`, `base64`, `base64url`, `latin1`/`binary`,
`ascii`, `ucs2`/`ucs-2`/`utf16le`/`utf-16le`, any case; arrays, array-likes, `{type: "Buffer", data}`, a Uint8Array
with an offset and length), `Buffer(...)` and `new Buffer(...)`, `alloc` (with a fill and its encoding),
`allocUnsafe`, `allocUnsafeSlow`, `byteLength`, `concat`, `compare`, `isBuffer`, `isEncoding` and `poolSize`;
`toString(encoding, start, end)`, `toJSON` (`{type: "Buffer", data}`, so `JSON.stringify` and a returned Buffer give
that), `slice`/`subarray` (Buffers over the same memory), `equals`, and, handed to the host Buffer upstream, so with
Node's argument checks and error texts as Bun gives them (`code` `ERR_OUT_OF_RANGE`, `ERR_INVALID_ARG_TYPE`,
`ERR_UNKNOWN_ENCODING`, `ERR_INVALID_ARG_VALUE`, `ERR_INVALID_BUFFER_SIZE`, `ERR_BUFFER_OUT_OF_BOUNDS`): `compare`,
`indexOf`, `lastIndexOf`, `includes`, `write`, `fill`, `copy`, every `read*`/`write*` integer and float method
(`readUIntLE(offset, byteLength)` and the like up to 6 bytes) and `swap16`/`swap32`/`swap64`. Text decoding replaces
invalid UTF-8 as WHATWG does, base64 decoding is Node's forgiving decoder (either alphabet, other characters skipped,
stopping at `=`), hex stops at the first pair that is not one. `console.log` shows `<Buffer 68 69>` (50 bytes at
most), `String(buffer)` its UTF-8 text. A Buffer crosses to an extension as a `HostValue::Buffer` and one coming back
is a Buffer again; a tool refuses it like any Uint8Array. `structuredClone` copies it as a plain Uint8Array, as
upstream does. Every behaviour above is checked against upstream 2.3.1 (`tests/upstream_2_3_1.rs`).

Since 2.4.1: `slice`, `subarray`, `map` and `filter` make a Buffer from a Buffer, also called through
`Uint8Array.prototype` (#64933); `BYTES_PER_ELEMENT` is 1 on `Uint8Array`, its prototype and `Buffer` (#64930);
`Uint8Array.fromBase64(text, { alphabet, lastChunkHandling })` and `toBase64({ alphabet, omitPadding })` take the
spec's options (`"base64"`/`"base64url"`, `"loose"`/`"strict"`/`"stop-before-partial"`), an option of another type
or value failing with `TypeError: Uint8Array.fromBase64 requires that alphabet be "base64" or "base64url".` and the
like (#64931).

## Error stacks (2.4.1, #64939)

Every Error has an own non-enumerable `stack` in V8's format (`interp::stack`): `name: message` (read when `stack` is
first read, `name` and `message` as data, a program getter reading as undefined), then up to 10 lines `    at fn
(<program>:line:col)`, innermost first, taken when the error is made (`at <program>:line:col` for top-level code;
`<anonymous>` for a nameless function). Each running function's line points at the call, `new` or `await` it last
reached. An async function resumed by a job, or a generator resumed by `next()`, keeps only its own frames on top of
whatever resumes it. An interpreter failure caught as an Error has one line where it was raised; a tool or host
failure, one line where the catching code observed it. Assigning or deleting `stack` replaces it.

## The `path` global (upstream 2.3.1, #64476, `experimental.full_codemode` only)

`path::extension(directory)` is core's `CodeModePath.extension` for a run in the session's `directory`: a global
`path` (also `import`able and `require`able as `path`/`node:path`) holding Node's `path` module as Bun 1.4.4 gives it,
the host's flavour (`Flavour::host()`: POSIX, or Win32 on Windows) with `path.posix` and `path.win32`. Only functions
and separators cross (`posix`/`win32` point back at their module upstream, which a global can't hold, so each is a
copy of its own functions without them): `resolve`, `normalize`, `isAbsolute`, `join`, `relative`,
`toNamespacedPath`, `dirname`, `basename`, `extname`, `format`, `parse`, `sep`, `delimiter`, `_makeLong` and
`matchesGlob`, in that order, named as Bun binds them (`bound join`, length 1; `resolve` length 0 and `relative`
length 2 are core's own). There is no `process.cwd()`: the host's flavour's `resolve`/`relative` start from
`directory`, and the other flavour, which can't read that directory as absolute, from its own root (`/` or `\`).
Calls fire the extension hooks as `path.join`, `path.win32.resolve`, ... (core's progress rows list only `fetch`).

`path::Module::new(flavour, cwd).call(name, &args)` is the module itself: a port of Node's `lib/path.js` on UTF-16
units, with Bun's argument checks (`TypeError: The "paths[0]" property must be of type string, got number`, Node's
`The "pattern" argument must be of type string. Received ...` for `matchesGlob`) and Bun's differences
(`win32.toNamespacedPath` returns the resolved path, which for a relative path reads the server's working directory;
`format` adds the dot to an `ext` without one). `matchesGlob` is Bun's glob (`glob::matches`): `*`, `**` (two stars
only, as a whole segment or joined by `/**`), `?`, classes, braces expanded first, `@(..)`/`?(..)`/`+(..)`/`*(..)`/`!(..)`,
leading `!` negation, escapes with `\b`, `\n`, `\r`, `\t` as control characters; `win32.matchesGlob` reads every
backslash of the path and the pattern as `/`. `path::extension_for(flavour, directory)` and `extension_in(flavour,
directory, cwd)` build it as another host would (tests run both flavours everywhere).

Not reproduced: Bun 1.4.4 keeps state between `matchesGlob` calls of different flavours (a pattern just used by
`win32.matchesGlob` can match as Win32 in `posix.matchesGlob`); here each call stands alone, as in a fresh process.
About 1 in 5 000 generated nonsense patterns still differs (a run of six stars after `**/`).

### What core's `full_codemode` takes from this crate

- The `path` global: `oc_codemode::path::extension(session_directory)` in the runtime's extensions, given to the run
  only with the flag (`select_extensions`, or a runtime made with it).
- Object globals, constructible globals, functions passed back, `HostCall::Block` and per-run selection: the host API
  above, for anything else core moves into Code Mode.
- Background slow scripts (#64343) need nothing more from the runtime: core runs `execute` as a job and drops its
  future to cancel it (dropping interrupts the program and its host work), and suspend/resume is not used by core.

## Unsupported JavaScript (same as upstream, same diagnostics)

Classes and private fields, user-defined constructors (`new f()` on a program function), getters/setters in object
literals, BigInt, arbitrary `Symbol()` values and symbol-keyed properties (only `Symbol.iterator`/`asyncIterator`),
exports and `import.meta`, `new.target`, `super`, `with`, `debugger`, `eval`/`Function`, timers (`setTimeout`,
`queueMicrotask`), `Proxy`/`Reflect`, `Object.defineProperty`/property descriptors, `for...in` skipping keys deleted during the loop,
`ArrayBuffer`/`DataView`/typed arrays other than `Uint8Array`, `Request`/`Response`/`Blob`, `crypto.subtle`, non-UTF-8
`TextDecoder`, `Iterator.concat`/`zip`, array methods on array-likes (`Array.prototype.slice.call(arguments)`),
TypeScript syntax. Unsupported syntax is reported lazily (when reached) as `UnsupportedSyntax`.

### Automation code

- `scan::scan_tools(code)` is `CodeMode.scanTools`: the `tools.<path>` a program calls statically, in first-seen
  order, or the diagnostic for dynamic bracket access, aliasing/passing `tools`, loading `tools` (or a computed
  module name) with `import`/`require`, a syntax error or empty code (cases captured from upstream 2.3.1 in
  `tests/fixtures/scan_tools.jsonl`). The diagnostics say "Automation code must …" as in 2.3.1.
- `scan::scan_program(code, globals, forwarding)` is 2.4.1's `CodeMode.scanProgram` (#65236): the tools, plus each
  unshadowed reference to one of `globals` (its member, and the members later read on a call's result, through
  `forwarding` members such as `slice`; `None` when the value escapes) and the literal modules loaded. Core reads it
  for `fetch` and `Bun` to authorize an automation's extension permissions. Import specifiers declare bindings.
- `CodeMode::with_bindings(vec![(name, json)])` binds read-only JSON globals (upstream `execute(code, bindings)`),
  which top-level declarations may shadow.
- `parser::validate_regexp(pattern, flags)` exposes acorn's regular expression validation.

## Known differences from upstream

- A timeout that fires while a tool's uninterruptible cleanup is still running upstream (Effect waits for it) cannot
  occur here: host futures are dropped immediately, so no `TimeoutExceeded` warning is produced in that case.
- Strings with lone surrogates become U+FFFD when they cross to the host as JSON (Rust strings are UTF-8); inside the
  program, including `JSON.stringify`, they are preserved.
- Arrays are dense: an index write past 10 000 000 elements throws `RangeError: Invalid array length` instead of
  creating a sparse array.
- Exact JS stack-depth overflows of the host engine are approximated: recursion, async recursion included, stops at
  upstream's 10 000-call limit with a located `RangeError`, and source nested deeper than the parser's limit (10 000
  levels, member/call chain links included) fails with upstream's "Execution exceeded the maximum nesting depth."
  before running, where upstream fails when its evaluation overflows Bun's stack.
- Upstream's runner yields to the host every millisecond of a long job (so other Effect fibers keep running); here
  each execution has its own thread, so there is nothing to yield to. A synchronous generator body that awaits
  across its caller (only possible through `search(...)` arguments holding promises inside a generator) is not
  supported.
- `scan_tools` walks the parser's ESTree JSON, where node kinds outside the interpreter's subset (classes,
  `new.target`, `import.meta`) carry no children, so a `tools.<path>` call inside a class body is not found (upstream finds it; the
  program fails when it runs either way).

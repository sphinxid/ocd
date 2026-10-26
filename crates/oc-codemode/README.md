# oc-codemode

Rust port of opencode's `@opencode/codemode` (upstream `packages/codemode`, plus the model-facing glue from
`packages/core/src/codemode`): a confined, tree-walking interpreter for a JavaScript subset that lets a model script
host tools from a single `execute` tool call.

Programs can only use the supported JavaScript built-ins, the `tools` tree, the synchronous `search(...)` discovery
function, and globals the host adds through extensions (core adds only `fetch`). Classes, getters/setters, BigInt,
arbitrary symbols, timers, imports, `eval`/`Function`, and TypeScript syntax are rejected with the same diagnostics as
upstream. The exact support matrix is upstream's `interpreter-support.md`; behaviour, diagnostics, and model-visible
strings match upstream byte for byte (verified by replaying upstream's recorded test executions, see Tests).

## Architecture

| Module | Role (upstream source) |
| --- | --- |
| `parser` | Port of acorn 8.15 (`ecmaVersion: "latest"`, script, top-level `return`/`await`), including its regex validator; exact acorn error messages and UTF-16 positions |
| `ast` | ESTree-shaped AST |
| `value`, `object`, `number` | Values (UTF-16 strings, f64, two confined symbols), objects with prototype chains and JS property order, ECMAScript number formatting/parsing (`objects.ts`) |
| `interp::eval` | Evaluator (`interpreter.ts`): statements, expressions, closures, destructuring, generators, async functions |
| `interp::exec` | Single-threaded fiber runtime reproducing Effect's scheduling (eager forks, synchronous resumption, `yieldNow` FIFO) |
| `interp::promises`, `generators`, `iteration`, `coerce`, `callback`, `errors`, `globals`, `extensions` | Promise registry and combinators, generator protocol, iterator cursors, ToPrimitive, callback admission, diagnostics, global table, host-value boundary |
| `stdlib::*` | Standard library (`stdlib/*.ts`): Object, Array, String, RegExp (`regress`), Number, Math, Date, JSON, Map/Set/Weak*, Iterator, URL/URLSearchParams, Headers, Uint8Array, TextEncoder/Decoder, atob/btoa, crypto, structuredClone, console |
| `data` | JSON boundary (`data.ts`): `toJson`/`toBoundary`/`fromJson`, JS-compatible JSON text |
| `tool_runtime`, `tool_schema` | Tool trie, `search`, admission/limits/hooks (`tool-runtime.ts`); JSON Schema → TypeScript signatures (`tool-schema.ts`) |
| `codemode` | Public runtime API, limits, output bounding (`codemode.ts`, `interpreter/execute.ts`) |
| `catalog`, `execute_tool`, `web` | Core glue: catalog summarization and instruction texts (`core/src/codemode/{catalog,instructions}.ts`), the `execute` tool's description/schema/result formatting/progress rows (`codemode/tool.ts`), and the `fetch` extension (`codemode/web.ts`) |
| `openapi` | OpenAPI 3.x → tools adapter (`openapi/*.ts`; unused by core upstream) |

Each execution runs on a dedicated thread (1 GiB stack, so deep recursion up to the 10 000-call limit is safe) with
its own executor; host futures (tool calls, `fetch`) are polled there inside the caller's tokio runtime context and
wake the interpreter through a thread-safe queue. When the program returns, every pending fiber is dropped (tool
`after` hooks see `Interrupted`), and all objects of the run are torn down to release reference cycles.

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
- `ExecResult::{Success { value, warnings, logs, truncated, tool_calls }, Failure { error, logs, truncated,
  tool_calls }}`; `Diagnostic { kind: DiagnosticKind, message, location: Option<Location { line, column }>,
  suggestions }` with the eleven upstream kinds (`ParseError`, `UnsupportedSyntax`, `UnknownTool`,
  `InvalidToolInput`, `InvalidToolOutput`, `InvalidDataValue`, `ToolCallLimitExceeded`, `TimeoutExceeded`,
  `ToolFailure`, `ExecutionFailure`, `Truncated`).
- Tool paths: dots create namespaces; the last tool for a path wins; a path may be both a tool and a namespace.
  `ToolDef.input`/`output` are `tool_schema::ToolSchema::Json` (render-only JSON Schema) or `ToolSchema::Document`
  (a pre-built schema document with top-level definitions). `output: None` renders `Promise<void>` and the program
  receives `undefined`.
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

Extensions (`host::Extension`, `SimpleExtension`) expose `HostFunction`s as globals. Values cross as `HostValue`
copies (plain data, Date, RegExp, URL, URLSearchParams, Headers, Map, Set, bytes, errors with name/message/cause/own
fields, and nested host functions that become callable program functions). A host function returns
`HostCall::Ready` (synchronous) or `HostCall::Pending` (the program sees a promise).

HTTP: `host::{HttpClient, HttpRequest, HttpResponse}`; `web::ReqwestClient` is the default client.
`web::extension(client)` is the `web` extension exposing `fetch(url, { method?, headers?, body? })` with a 30 s
timeout and the upstream response shape `{ url, status, statusText, ok, redirected, headers: { get, has, entries },
text(), json(), bytes() }`.

### The `execute` tool and catalog instructions

- `execute_tool::{NAME, DESCRIPTION, input_schema()}` — the tool definition (verbatim upstream description).
- `execute_tool::format_result(&ExecResult)` — the text output (value as text/pretty JSON, or error message plus
  suggestions, then `Warnings:` and `Logs:` sections); `execute_tool::output(...)` builds the
  `{ output, toolCalls, error?, files }` structure; `ProgressRows` tracks `{ tool, status, input? }` progress rows
  from the host hooks (fetch rows show `{ method, url }`); `display_input`, `fetch_display`, `output_file`.
- `catalog::inventory(&catalog, &namespaces, &pinned)` builds the catalog tree; `catalog::summarize(&inventory,
  None)` applies the 2 000-token inline budget (chars/4 rounded; every namespace line shown; pinned listings always
  shown; remaining listings round-robin per namespace, cheapest first); `catalog::render(&summary)` produces the
  `core/codemode` instruction text; `catalog::update(&previous, &current)` the change delta (or replacement);
  `catalog::{EMPTY_CATALOG, REMOVED_CATALOG}`.

### OpenAPI (`openapi`)

`openapi::from_spec(options)` turns an OpenAPI 3.x document into `ToolEntry`s plus a `skipped` list;
`OpenApiHost` executes the generated tools over an `HttpClient` with host-resolved auth. See the module docs.

## Tests

`cargo test -p oc-codemode` (all green):

- `tests/upstream_replay.rs` — replays every program execution recorded from the upstream test suite
  (`tests/fixtures/upstream/`, generated by instrumenting upstream; format in that directory's README) and requires an
  identical `Result`: 1 704 of 1 707 recordings match; the 3 documented divergences are in
  `tests/fixtures/replay-skips.txt`. The recordings come from 2.0.26; replaying their tool-free programs through
  upstream 2.1.4 changes only the one skipped for async recursion depth.
- `tests/upstream_2_1.rs` — the cases upstream added up to 2.1.4: jobs run to completion, the `Uint8Array.from`
  mapper, non-coercing byte searches, `flatMap` holes, the host's `Number#toString` radix check, `fetch` refusing
  `file:` URLs, the fetch display method fallback, and recursion depth as 2.1.4 reports it.
- `tests/extensions_replay.rs` (42 recordings) and `tests/hooks.rs` (9 recordings) — extension host functions and
  call hooks, including hook ordering and denials.
- `tests/test262_sample.rs` — a deterministic quarter (3 333 files) of upstream's test262 conformance runs with a port
  of its harness; all identical except 3 documented in `tests/fixtures/test262-skips.txt`.
- `tests/parser_acorn.rs` — parser vs acorn 8.15 on 2 127 snippets and all 15 008 vendored test262 files (AST and
  exact errors; regenerate with `node tests/fixtures/parser/gen.js`).
- `tests/signatures.rs` — every recorded tool signature, catalog read, schema-rendering call, `toolExpression`, and
  `searchSignature` against upstream.
- `tests/catalog.rs` — catalog summarization, instruction render and update texts against goldens produced by
  upstream core.
- `tests/stdlib_*.rs` — per-module parity cases whose expectations were produced by upstream (array, collections,
  iterator, string/regexp, number/math, date incl. other time zones, url/headers/bytes/web).
- `tests/openapi.rs` — every recorded `OpenAPI.fromSpec` call and HTTP round trip, plus 49 extra upstream-generated
  cases.
- `tests/fetch.rs`, `tests/teardown.rs`, and unit tests in `number`, `tool_schema`, `catalog`, `execute_tool`.

## Unsupported JavaScript (same as upstream, same diagnostics)

Classes and private fields, user-defined constructors (`new f()` on a program function), getters/setters in object
literals, BigInt, arbitrary `Symbol()` values and symbol-keyed properties (only `Symbol.iterator`/`asyncIterator`),
`import`/`import()`/modules, `new.target`, `super`, `with`, `debugger`, `eval`/`Function`, timers (`setTimeout`,
`queueMicrotask`), `Proxy`/`Reflect`, `Object.defineProperty`/property descriptors, `for...in` over inherited keys,
`ArrayBuffer`/`DataView`/typed arrays other than `Uint8Array`, `Request`/`Response`/`Blob`, `crypto.subtle`, non-UTF-8
`TextDecoder`, `Iterator.concat`/`zip`, array methods on array-likes (`Array.prototype.slice.call(arguments)`),
TypeScript syntax. Unsupported syntax is reported lazily (when reached) as `UnsupportedSyntax`.

## Known differences from upstream

- A timeout that fires while a tool's uninterruptible cleanup is still running upstream (Effect waits for it) cannot
  occur here: host futures are dropped immediately, so no `TimeoutExceeded` warning is produced in that case.
- Strings with lone surrogates become U+FFFD when they cross to the host as JSON (Rust strings are UTF-8); inside the
  program, including `JSON.stringify`, they are preserved.
- Arrays are dense: an index write past 10 000 000 elements throws `RangeError: Invalid array length` instead of
  creating a sparse array.
- `(?i:...)` regex modifiers apply case-insensitivity to `\p{...}` as the spec requires; JavaScriptCore does not.
- Exact JS stack-depth overflows of the host engine are approximated: recursion, async recursion included (upstream
  2.1 runs each job to completion, so it no longer overflows the host stack first), stops at upstream's 10 000-call
  limit with a located `RangeError`.

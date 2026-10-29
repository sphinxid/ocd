# oc-server

The opencode v2 HTTP API server, wire-compatible with `assets/openapi.json` (OpenAPI 3.1,
162 operations, 350 component schemas; v2.2.2's document) so the upstream TypeScript client/SDK/TUI and the Rust
TUI work unchanged. Behaviour follows `docs/spec/06-http-api.md`.

The engine is **not** a dependency: everything it must do is expressed by the generated
[`Backend`](src/generated.rs) trait. This crate owns the transport — routing, request decoding
and 400 shapes, envelopes, the error table, auth, pairing, PTY tickets, CORS, readiness,
compression, SSE framing, raw file reads, PTY WebSockets and web-UI serving.

## Public API

```rust
use std::sync::Arc;
use oc_server::{serve, ServerOptions, Backend};

let server = serve(Arc::new(MyEngine::new()) as Arc<dyn Backend>, ServerOptions {
    hostname: "127.0.0.1".into(),   // default
    port: None,                     // None: try 4096 and count up while busy; Some(0): ephemeral
    password: Some(pw),             // None/empty: unauthenticated mode
    cors: vec![],                   // extra allowed origins (`--cors`)
    version: "2.1.4".into(),        // /api/info version ("unknown" if empty)
    tmp_dir: "/tmp/opencode".into(),// /api/info paths.tmp
    cwd: None,                      // default location (process cwd)
    web_ui: None,                   // Some(WebUiSource): SPA on non-API paths (see "Web UI")
    ready: false,                   // start in "starting" state
}).await?;

println!("server listening on {}", server.url());
// … build the engine …
server.set_ready();                 // or server.set_failed() if boot failed
server.meta().set_extra_urls(vec![tunnel_url]); // appended to /api/info urls
server.shutdown().await;            // or server.wait().await
```

* `serve(backend, opts) -> anyhow::Result<RunningServer>` binds **before** the app is ready and
  answers immediately: until `set_ready()`, `GET /api/info` returns `503` + `retry-after: 1`
  with `{version,pid,urls,paths}` (no `capabilities`), every other authenticated request gets
  `503 {"code":"service_starting"}`; after `set_failed()` `/api/info` is `500` and other routes
  `503 {"code":"service_failed",message,action}`; during `shutdown()` `service_stopping`.
* `RunningServer`: `url()`, `local_addr()`, `urls()`, `meta()`, `status()`, `set_ready()`,
  `set_failed()`, `shutdown()`, `wait()`.
* `router(backend, &opts) -> (axum::Router, RouterHandle)` builds the same application without
  binding (tests / embedding). `RouterHandle` has `set_ready/set_failed/set_stopping/meta/issue_session`.
* `oc_server::service` — service-mode helpers: `default_service_port(channel)` (`0xC0DE`
  latest/dev/beta/next, `0xC0DF` local, else sha1-derived; on Windows a port of `0x8000` or more
  becomes `0x7000 | (port & 0xfff)`, so 28894/28895), `service_file_name`,
  `default_registration_path`, `Registration` + `write_registration` (atomic, 0600) /
  `read_registration` / `remove_registration_if_owned` / `watch_registration` (5 s watchdog),
  `generate_password()` (32 random bytes, base64url). The election/incumbent probing itself is
  the CLI's job.
* `OPENAPI_JSON` — the document served at `/openapi.json`.

## The `Backend` trait

Generated in `src/generated.rs`: **one async method per operationId** (snake_case of the dotted
id, e.g. `session.revert.stage` → `session_revert_stage`,
`experimental.persistentPty.read` → `experimental_persistent_pty_read`), 158 in
total (162 operations minus 4 the server owns: `server.pair`, `server.connect`,
`pty.connect.token`, `experimental.persistentPty.connectToken`).

```rust
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// `GET /api/agent/{agentID}` — `agent.get`. Scope `L`, envelope `Location`.
    async fn agent_get(&self, ctx: &RequestContext, req: AgentGetRequest)
        -> Result<AgentInfo, ApiError> { Err(ApiError::not_implemented("agent.get")) }

    /// `POST /api/session/{sessionID}/prompt` — `session.prompt`. Scope `S`, envelope `Data`.
    async fn session_prompt(&self, ctx: &RequestContext, req: SessionPromptRequest)
        -> Result<SessionInboxUser, ApiError> { … }

    /// `DELETE /api/session/{sessionID}` — 204.
    async fn session_remove(&self, ctx: &RequestContext, req: SessionRemoveRequest)
        -> Result<(), ApiError> { … }
    // … one per operation …
}
```

* **Every method has a default body** returning `ApiError::not_implemented` (HTTP 500, empty
  body), so an engine can implement operations incrementally. `server_info`'s default builds
  `ServerInfo` from `ctx.server` with `capabilities: {persistentPty: false}` and the machine
  `hostname` (`os.hostname() || undefined`; the pre-ready 503 body has neither).
* **Request** structs `<OpPascal>Request` carry the decoded path params, query params and
  `body` (field names snake_case of the wire names). Path/query decoding is done by the server:
  ID patterns (`^ses`, `^msg_`, `^pty`, …), `"true"/"false"` booleans → `bool`,
  `NumberFromString` ints with upstream's bounds (`limit` ≥ 1, message `limit` ≤ 200, PTY read
  `lines` ≤ 65535, …) → `i64`, enums → generated enums. `location[directory]` is **not** a
  field — it is resolved into `ctx.location`.
* **Return values are payloads.** The server wraps them in the operation's envelope:
  `Location` → `{"location":{"directory":ctx.location.directory},"data":…}`, `Data` →
  `{"data":…}`, `Bare` → as is, `NoContent` → `204` with an empty body and no content-type.
  The envelope and scope of every operation are listed in its doc comment and in `OPS`.
* **`RequestContext`**: `location: LocationRef` (resolved `?location[directory]` →
  `x-opencode-directory` header (URI-decoded, raw on failure) → `opts.cwd`, verbatim),
  `scope`, `operation` (operationId), `headers`, `server: Arc<ServerMeta>` (version, pid,
  urls, tmp).
  * Scope `L`: the server already verified the directory exists (else `404
    LocationNotFoundError`), before decoding params — like upstream's location middleware.
  * Scope `S` (session) / `F` (forms): the server validated `sessionID` starts with `ses`
    (else `400 {"message":"Invalid session ID","field":"sessionID"}`); `F` with
    `sessionID == "global"` uses `ctx.location` (verified). The backend resolves the session's
    own location and answers `SessionNotFoundError` / `LocationNotFoundError`.
  * Scope `G`/`H`: global; `ctx.location` is informational (e.g. `debug.location.evict`).
  * Exceptions from the spec the backend must honour itself: `session.create` /
    `experimental.session.import` use `body.location ?? cwd` (not the header);
    `experimental.generate.text` runs at the global config directory.

### Operations with hand-written transport

| operation | signature | notes |
|---|---|---|
| `session.list` | `(ctx, SessionListRequest{limit, query}) -> Vec<SessionInfo>` | server decodes/encodes base64url JSON cursors (`{order?,search?,parentID?,directory|project,subpath?,anchor:{id,time,direction}}`); `query.anchor` is set when paging; response `{data, cursor:{previous,next}}` with `null`s on an empty page; bad cursor → `400 InvalidCursorError` |
| `session.message.list` | `(ctx, SessionMessageListRequest{session_id, limit, order, type, cursor}) -> Vec<SessionMessageInfo>` | cursor `{id,order,direction}`; `cursor` + `order` → `400 "Cursor cannot be combined with order"` |
| `experimental.session.log` | `(ctx, ExperimentalSessionLogRequest{session_id, after, follow}) -> SessionLogStream` | `BoxStream<Result<Value, StreamFailure>>`; frames `data: <json>`; an `Err` emits `event: effect/http-api/stream/failure` + `data: <cause array>` and ends; errors before streaming are JSON |
| `event.subscribe` | `(ctx, EventSubscribeRequest{location: None}) -> EventSubscription` | `Broadcast(broadcast::Receiver<Value>)` or `Stream(BoxStream<Value>)` of bus payloads; the server decodes `?types=` (repeated or single; public types or `rpc.*`, else `400` `kind: Query`) and filters per subscriber |
| `experimental.media.get` | `(ctx, ExperimentalMediaGetRequest{hash}) -> MediaBlob{mime, data}` | hash `^[0-9a-f]{64}$`; the server adds `cache-control: private, max-age=31536000, immutable`, `etag: "<hash>"`, `x-content-type-options: nosniff`, `content-security-policy: default-src 'none'; sandbox`, the mime for png/jpeg/gif/webp else `application/octet-stream` + `content-disposition: attachment`, and answers a matching `If-None-Match` (`*`, `W/`, lists) with 304 |
| `fs.read` | `(ctx, FsReadRequest{path}) -> FsFile{size, mtime_ms, mime, source: Path|Bytes}` | server does ETag / Last-Modified / If-None-Match / If-Modified-Since / single Range / If-Range (200/206/304/416) |
| `pty.connect` | `(ctx, PtyAttachRequest{pty_id, cursor}) -> PtyAttachment{replay, cursor, output, control}` | `Err(ApiError::pty_not_found)` → close 4404 "session not found"; `Err(ApiError::pty_exited())` → 4404 "session exited" |
| `experimental.persistentPty.connect` | `(ctx, PersistentPtyAttachRequest{pty_id, cursor, attachment_id, role, takeover}) -> PersistentPtyAttachment{info, role, generation, replay, events, control}` | any `Err` → close 4404 "terminal unavailable" |
| `rpc.call` | generated | a `Defect` maps to `500 RpcInternalError{type:"rpc.internal",message:"RPC call failed"}` |

### Errors (`ApiError`)

* `ApiError::Typed{status, body}` — a declared `_tag` error, built with constructors carrying
  upstream's exact message templates: `session_not_found(id)` → `Session not found: <id>`,
  `location_not_found(d)`, `agent_not_found`, `pty_not_found` (`PTY session not found: <id>`),
  `shell_not_found`, `permission_not_found`, `form_not_found`, `message_not_found`,
  `session_busy`, `conflict(msg, resource)`, `invalid_request(msg).with_kind(..).with_field(..)`,
  `invalid_cursor`, `rpc`, `rpc_internal`, `worktree(msg, force_required)`,
  `instruction_entry_too_large`, `unknown(cause)` (`UnknownError` with `ref: err_xxxxxxxx`),
  `service_unavailable(msg, service)`, `vcs_init_not_supported`, … or `ApiError::typed(x)` for
  any generated `*ErrorEncoded` value (`ErrorEncoded::TAG/STATUS`). Keys are emitted `_tag`
  first in schema order; absent optional fields are omitted.
* `ApiError::Defect(msg)` — `500` with an empty body (logged).
* `ApiError::Empty(status)` / `ApiError::Text{status, body}` — bare statuses / `text/plain`.

Schema decode failures produce `400 {"_tag":"InvalidRequestError","message":…,"kind":"Params"|"Query"|"Payload"}`
(message truncated at 1024 chars + `... (<n> more chars)`; text is best-effort, not
Effect-identical). Non-JSON content types on JSON endpoints → `415 Unsupported content-type: <ct>`;
a missing content-type is treated as `application/json`; an empty JSON body is a `Payload` error.

## Generated types

`codegen/gen.py` reads `assets/openapi.json` and writes `src/generated.rs` (committed;
deterministic). Regenerate with `python3 crates/oc-server/codegen/gen.py`.

* One Rust type per component schema (`Session.Message.Info` → `SessionMessageInfo`), plus
  named types for inline objects/enums/unions (`SessionInfoTime`, `SessionPromptBody`, …).
* Struct fields keep schema property order (= JSON key order) with `#[serde(rename)]`.
* required → `T`; required + nullable → `Option<T>` serialized as `null`; optional →
  `Option<T>` omitted when `None` (both absence and `null` accepted on input).
* Unions (`anyOf`) → `#[serde(untagged)]` enums; discriminator fields (`type`, `status`,
  `_tag`, …, single-value enums) are zero-sized `LitXxx` markers that only accept their
  literal, so variants are picked exactly. `From<Variant>` impls are generated.
* `number` → `f64` serialized like `JSON.stringify` (integral values without `.0`);
  `number | "Infinity" | "-Infinity" | "NaN"` → `JsNumber`; `{type: object}` → `JsonObject`;
  `additionalProperties: X` → `IndexMap<String, X>`; `{}`/`not` → `serde_json::Value`.
  Unknown input keys are ignored (as upstream).
* `Schema.ArrayEnsure` query parameters (`model.list ?types=`) → `Option<Vec<T>>`, from repeated
  keys or one value promoted to an array.
* v2.2's flattened `anyOf[…, number, "Infinity"|"-Infinity"|"NaN", …]` is nested back so it still
  maps to `JsNumber`.
* `OPS: &[OpMeta]` (id, method, path, axum route, scope, envelope) and the generic
  `dispatch` used by the router.

## Transport behaviour

* **Auth** (username `opencode`): `auth_token` query (base64 `user:pass`, takes precedence),
  `Authorization: Basic`, or the `opencode_session_<port>` cookie (same-origin only). The
  password or a pairing session token (`<expires>.<base64url HMAC-SHA256>`, key =
  `HMAC(password, "opencode-session-v1")`, 30 days) is accepted. `401
  {"_tag":"UnauthorizedError","message":"Authentication required"}` with `www-authenticate:
  Basic realm="Secure Area"` only when `sec-fetch-mode` is absent or `navigate`. Auth runs
  before routing (unknown `/api/*` and `/openapi.json` → 401). Exempt: `OPTIONS`,
  `/auth/connect/{code}`, PTY `connect` URLs with a non-empty `ticket` (only once ready), and
  web-UI paths. No password ⇒ unauthenticated mode.
* **Pairing**: `POST /api/pair` → `{code, expires_in:300}` (16 random bytes base64url,
  single-use, 5 min, 1000 entries). `GET /auth/connect/{code}`: `Accept` containing `text/html`
  → `302 Location: /` + `Set-Cookie: opencode_session_<port>=<token>; Max-Age=2592000; Path=/;
  HttpOnly; SameSite=Lax` (failure: `401 text/plain`); otherwise `{token}` (failure: 401 JSON).
* **CORS** (outermost): every `OPTIONS` → `204` with `access-control-allow-origin` (allowed
  origins only), `vary`, `access-control-allow-methods: GET, HEAD, PUT, PATCH, POST, DELETE`,
  echoed `access-control-allow-headers`, `access-control-max-age: 86400`; other responses
  (including 401s) get `access-control-allow-origin` for allowed origins. Allowed:
  `http://localhost:*`, `http://127.0.0.1:*`, `oc://renderer*`, tauri origins,
  `https://*.opencode.ai`, `opts.cors`. Disallowed origins are not blocked, except PTY
  token/connect routes (403 unless allowed or same host).
* **PTY tickets**: `POST …/connect-token` needs `x-opencode-ticket: 1` and an allowed/same-host
  Origin (else `403 ForbiddenError`), checks the PTY exists via `pty_get` /
  `experimental_persistent_pty_get`, issues a UUID v4 (60 s, single use, scope
  `{ptyID, directory}` / `{ptyID}`).
* **PTY WebSockets**: location PTY — replay as text frames of ≤ 65 536 UTF-16 units, then a
  binary `0x00{"cursor":n}` meta frame, live text frames, close `1000` at end; inbound text
  verbatim, binary strict-UTF-8. Persistent PTY — `attached` JSON, replay binary,
  `replay_complete`, live binary output / JSON events (`resized` with base64 checkpoint);
  `input_protocol=1` framed input `[type][cols BE][rows BE][payload]`.
* **SSE `/api/event`**: `content-type: text/event-stream`, `cache-control: no-cache,
  no-transform`, `x-accel-buffering: no`, `x-content-type-options: nosniff`; only
  `data: <JSON>\n\n` frames; first `server.connected` (`evt_` ascending id); only public event
  types (`ServerDefinitions`, `server.connected`, `rpc.*`) are forwarded; `: heartbeat` every
  15 s; a subscriber 4096 frames behind (or lagging its broadcast channel) is dropped and its
  response aborted.
* **Compression**: br/gzip/deflate for compressible types ≥ 1024 bytes (bodies without a
  known length — the web UI's — at any size); never SSE, `no-transform`, already-encoded
  bodies, 1xx/204/206/304. `Vary` follows Node's `compression` behind Effect's middleware: every
  compressible, transformable response with a body gets `Accept-Encoding` appended **after**
  CORS's `Origin` (`vary: Origin, Accept-Encoding`, also when not compressed or below the
  threshold); a handler's own `vary: accept-encoding` (web UI brotli assets) stays first
  (`accept-encoding, Origin`); bodiless answers get none.
* **Web UI** (`opts.web_ui`, `webui.rs`, upstream `cli/src/services/web-ui.ts`):
  `WebUiSource::Path(p)` (archive file or unpacked `dist` directory, loaded at startup) or
  `WebUiSource::Loaded(Arc<WebUi>)` (`WebUi::from_archive(Bytes::from_static(..))` for an
  embedded archive, `WebUi::empty()`, `WebUi::load(path)`; `.with_mime(fn)` installs the
  `content-type` lookup — callers pass upstream's `mime-types` table, the default is
  `mime_guess`). Archive format (`cli/src/app-archive.ts`): `[u32 LE indexLen][JSON
  {"path":[offset,len]}][brotli bodies]`, offsets relative to the body;
  `WebUi::encode_archive` writes it. Behaviour:
  * runs before auth/readiness for every path except `/api`, `/api/*`, `/auth/*`,
    `/openapi.json`; the path is `new URL(url, "http://localhost").pathname` (dot segments
    resolved), the key is that path without its leading `/`;
  * missing key under `_assets/` or `icons/` → empty `404` + `cache-control: no-store` (any
    method); other missing keys → `index.html` (SPA fallback, also for dotted names and the
    legacy `/assets/*`); no `index.html` (an empty web UI, like upstream `--skip-web-ui`) →
    empty `404`; then non-GET/HEAD → empty `405`;
  * headers: `content-type` from the lookup (no charset), `cache-control: no-cache` for
    `index.html`, `sw.js`, `registerSW.js`, else `public, max-age=31536000, immutable`,
    `content-security-policy` (`sha256` of the inline `oc-theme-preload-script` only on HTML),
    `x-content-type-options: nosniff`, `vary: accept-encoding` when the asset has stored brotli
    bytes (not for HTML), and those bytes as-is with `content-encoding: br` when
    `Accept-Encoding` takes `br` (q≠0) and the key is not `index.html`;
  * bodies have no size hint, so hyper frames them chunked (HEAD: no length) and compression
    has no threshold — byte-for-byte what upstream's Node server sends;
  * `.map` files of a directory source are skipped.
* Unknown routes and method mismatches → empty `404`.

## Not implemented / deviations

* `connectionURLs` for wildcard binds (`0.0.0.0`/`::`) reports only the loopback URL (no
  network-interface enumeration).
* Schema-error message texts are best-effort (clients key on `_tag`/`kind`).
* `fs.read` dates compare at second granularity (HTTP-date), like `Date.parse` of
  `toUTCString()`.
* Readiness of the service-mode election, remote tunnel, and stdio mode are CLI concerns (the
  helpers in `service` cover ports, registration and passwords).
* `/openapi.json` serves the checked-in client document (spec Q12).

## Tests

`cargo test -p oc-server`: unit tests (auth tokens/cookies, CORS predicates, cursor codecs,
Range/ETag, UTF-16 chunking, framed input, ID generator, SSE overflow/lag/heartbeat, JSON number
encoding, error shapes, registration files) and integration tests with a `MockBackend`
(`tests/common`): routing and envelopes, decoding errors, auth/pairing/cookies, CORS,
readiness, RPC mapping, fs.read, PTY tickets, web UI (archive/directory/empty, SPA fallback,
brotli passthrough, CSP, HEAD, 404/405, dot segments, wire framing), compression and `Vary`
(`tests/api.rs`); SSE and PTY
WebSockets over a real listener, port scanning (`tests/stream.rs`); JSON-schema validation of
success and error bodies against `assets/openapi.json` (`tests/schema.rs`).

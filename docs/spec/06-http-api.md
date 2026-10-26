# 06 — HTTP API (wire contract)

Status: spec for the Rust port. Source of truth is the opencode v2 TypeScript checkout
(`packages/protocol`, `packages/server`, `packages/cli`, `packages/client`). All `path:line`
citations are relative to the checkout root
(`$OCD_SCRATCH/src/opencode`).

> **Primary machine-readable source: `packages/protocol/openapi.json`** (OpenAPI 3.1, 575 KB,
> 142 operations, 282 component schemas). It is generated from `packages/protocol/src/client.ts`
> by `packages/protocol/script/generate-openapi.ts` and byte-identical copies live at
> `services/www/openapi.json` and `services/www/public/openapi.json`. The generated TS clients
> (`packages/client/src/promise/generated/{client,types}.ts`,
> `packages/client/src/effect/generated/client.ts`) are derived from the same declaration.
> **Every request/response body schema in this document is a summary; the OpenAPI component of
> the same name is normative.** This document focuses on behaviour the OpenAPI does not capture
> (auth, location routing, raw/streaming endpoints, error mapping, defaults, framing).

`V2_HTTP_API_AUDIT.md` is the human audit of the same surface; it says 139 endpoints
(V2_HTTP_API_AUDIT.md:4) but the current OpenAPI has 142 and includes operations the audit does
not list (`server.pair`, `server.connect`, `location.reload`, `credential.list/create`,
`integration.connect.external`, `experimental.fs.write`, `vcs.init`). Trust the OpenAPI.

---

## 1. Architecture / request pipeline

The HTTP surface is an Effect `HttpApi` named `"server"` built in
`packages/protocol/src/api.ts:159-198` and served by `HttpApiBuilder.layer(Api, {openapiPath:
"/openapi.json"})` (`packages/server/src/routes.ts:176`). Groups are added in this order
(api.ts:160-189): server, location, agent, plugin, session, message, model, generate, provider,
integration, mcp, credential, project, form, permission, fs, command, skill, rpc, event, pty,
persistentPty, shell, reference, worktree, vcs, debug, migration, websearch, config. Two
API-wide middlewares are attached last: `Authorization` then `SchemaErrorMiddleware`
(api.ts:197-198).

There are two hosts for the same router:

* **Node server process** (`packages/server/src/process.ts`, used by `opencode serve`). Layering
  from outside in, per request:
  1. `HttpMiddleware.cors(...)` (process.ts:74) — outermost; answers every `OPTIONS` itself.
  2. `HttpMiddleware.compression()` (process.ts:73).
  3. optional `transform` = CLI web-UI handler (`packages/cli/src/services/web-ui.ts:15-30`),
     serves the SPA for any path that is not `/api`, `/api/*`, `/auth/*`, `/openapi.json`.
  4. `dispatch` (process.ts:173-201): readiness gate + process-level Basic/cookie auth.
  5. The `HttpRouter` built from the HttpApi (Authorization middleware → SchemaError middleware →
     per-group location middleware → handler).
* **Embedded fetch handler** (`packages/server/src/fetch.ts:41-54`, also `workerd.ts`): router +
  CORS only (no compression, no readiness gate, no web UI). Auth only if `options.password`
  set (`createRoutes`, routes.ts:84-93); `createEmbeddedRoutes` never authenticates
  (routes.ts:99-105).

The Rust server MUST reproduce host (1)'s externally visible behaviour.

### 1.1 Scope (location) model

Most endpoints execute "at a location" (a project directory with its own lazily built service
graph, cached in `LocationServiceMap`, `packages/core/src/location-service-map.ts`). Scope codes
used in §7:

| Code | Middleware | How the location is chosen | Extra errors |
|---|---|---|---|
| **G** | none | global services only | — |
| **L** | `LocationMiddleware` (`packages/server/src/location.ts:89-100`) | `requestRef(request)` (§3) | 404 `LocationNotFoundError` if the directory does not exist (`location.ts:20-34`) |
| **S** | `SessionLocationMiddleware` (`packages/server/src/middleware/session-location.ts:16-30`) | path param `sessionID` → `Session.get` → that session's location (`Instance.provide(session)`) | 400 `InvalidRequestError{message:"Invalid session ID", field:"sessionID"}` if `sessionID` does not decode as `Session.ID` (prefix `ses`), 404 `SessionNotFoundError`, 404 `LocationNotFoundError` (`location.ts:64-69`) |
| **F** | `FormLocationMiddleware` (`packages/server/src/middleware/form-location.ts:17-40`) | like **S**, except literal `sessionID == "global"` uses `requestRef(request)` (undocumented MCP-elicitation sentinel) | same as S |
| **H** | none; handler resolves itself | see endpoint | — |

Note: group middleware is applied only to endpoints added *before* `.middleware(...)` is called;
e.g. `pty.connect` is added after it and is raw (protocol/src/groups/pty.ts:120-146).

---

## 2. Wire conventions

### 2.1 JSON codec

* Request and response bodies are `application/json` (UTF-8). Effect HttpApi encodes/decodes
  through `Schema.toCodecJson`, which maps TypeScript `undefined` ↔ JSON `null`
  (effect `SchemaAST.ts` `undefinedToNull`). Consequences the Rust port must copy:
  * Fields declared with Effect `Schema.optional(X)` (shown in OpenAPI as `anyOf:[X,{type:null}]`)
    **accept `null` or absence on input**, and on output a key that is present-but-undefined is
    emitted as `null` (e.g. `SessionsResponse.cursor.previous` is `null` on an empty page,
    `packages/server/src/handlers/session.ts:77-100`).
  * Fields declared with the opencode helper `optional(X)` (`packages/schema/src/schema.ts:18-24`;
    OpenAPI shows plain `X`, not nullable) are **omitted** when undefined on output and
    accept absence on input.
* Unknown properties in request bodies: Effect's default decode ignores excess keys (OpenAPI
  says `additionalProperties:false`, but the server does not reject). See Open Question Q3.
* Output excess keys are stripped by encoding: e.g. handlers build `Location.Info` (with
  `project`) but the `Location.PublicRef` schema emits only `{directory}`.
* Success status is exact per endpoint: `200` with JSON body, or `204` with **empty body** and no
  content-type (`HttpApiSchema.NoContent`). The generated client rejects any other success status
  (`packages/client/src/promise/generated/client.ts:350-353`) and requires
  `content-type: application/json` (or `+json`) for JSON (client.ts:2265-2283).

### 2.2 Response envelopes (three shapes — must match per endpoint)

1. **Location envelope** for most **L** endpoints: `{"location":{"directory":"/abs"},"data":<T>}`
   (`packages/schema/src/location.ts` `response()`; built by `packages/server/src/location.ts:49-60`).
   Only `directory` is serialized (no `project`, no `workspaceID`).
2. **Data envelope** `{"data":<T>}` for session-scoped and some global endpoints.
3. **Bare** body: `GET /api/info`, `project.*`, `config.*`, `worktree.*`, `debug.location.list`,
   `experimental.migration.v1.status`, `session.interrupt` (`{interrupted}`),
   `persistentPty.handoff` (`{handoff}`), `rpc.call` (`{output?}`), `SessionsResponse`,
   `SessionMessagesResponse` (`{data, cursor}`).

The per-endpoint shape is given in §7 ("Response").

### 2.3 Query parameters

* `location` is a deepObject: `?location[directory]=/abs/path` (OpenAPI `style: deepObject`,
  `packages/protocol/src/groups/location.ts:6-27`). The client encodes nested objects as
  `key[child]=value`, `null` as the literal string `null`, arrays as repeated keys
  (`client.ts:2248-2263`).
* Numbers arrive as strings (`Schema.NumberFromString`), e.g. `limit=20`. Positive-int checks
  apply where noted in OpenAPI (`PositiveInt`, `NonNegativeInt`).
* Booleans in query are the literal strings `"true"`/`"false"` only
  (`BooleanFromString`, `packages/protocol/src/groups/session.ts:161-166`); anything else is a
  400 schema error.
* `session.list` `parentID=null` (literal string) means "root sessions only"
  (session.ts:48-58).

### 2.4 IDs

IDs are prefixed strings validated by pattern (from OpenAPI): `Session.ID` `^ses`,
`Session.Message.ID` `^msg_`, `Pty.ID` `^pty`, `Event.ID` `evt_…` (ascending), etc. A
path/query ID failing its pattern → 400 `InvalidRequestError{kind:"Params"|"Query"}` via the
schema-error middleware (§6.2), except inside **S/F** middleware where a bad `sessionID` yields
`{message:"Invalid session ID", field:"sessionID"}`.

### 2.5 Pagination cursors

* **Sessions** (`GET /api/session`): cursor = base64url(JSON(`{...originalQueryWithoutLimit,
  anchor:{id, time:<updated epoch ms>, direction:"previous"|"next"}}`)), where the query part is
  one of `{directory, order?, search?, parentID?}`, `{project, subpath?, ...}` or
  `{order?, search?, parentID?}` (`packages/protocol/src/groups/session.ts:84-125`). The server
  emits `previous` anchored at the first item and `next` at the last item of every non-empty page
  (`packages/server/src/handlers/session.ts:62-103`). Undecodable → 400
  `InvalidCursorError{message:"Invalid cursor"}`. When `cursor` is supplied, the other filter
  params are taken from the cursor, `limit` from the query (default 50).
* **Messages** (`GET /api/session/{id}/message`): cursor = base64url(JSON `{id, order,
  direction}`) (`packages/server/src/handlers/message.ts:11-26`); default `limit` 50, `order`
  `desc`; `cursor` together with `order` → 400 `InvalidCursorError{"Cursor cannot be combined with
  order"}`.
* **Shell output**: absolute byte cursor (`shell.output` query `cursor`, `limit`).
* **PTY**: absolute output cursor (§9).
* **Session log**: `after` = exclusive aggregate sequence number (§8.2).

---

## 3. Location selection

`requestRef(request)` (`packages/server/src/location.ts:71-79`):

1. query `location[directory]` if non-empty, else
2. header `x-opencode-directory`, `decodeURIComponent`-decoded (falls back to the raw value if
   decoding throws), else
3. `process.cwd()` of the server.

No normalisation except on Windows (`path.normalize`, `LocationServiceMap.canonical`,
`packages/core/src/location-service-map.ts:39-44`); the string is used as the cache key verbatim
(trailing slash ⇒ different location). Directory not found while building services →
`404 {"_tag":"LocationNotFoundError","location":{"directory":"<d>"},"message":"Location not found: <d>"}`.

`session.create`/`experimental.session.import` use `body.location ?? {directory: process.cwd()}`
— they do **not** consult the header (`handlers/session.ts:137-139,152`).
`experimental.generate.text` always uses the location whose directory is the global config dir
(`handlers/generate.ts:14-16`).

---

## 4. Authentication

### 4.1 Credentials

Username is fixed to `opencode` (`packages/server/src/auth.ts:19-21`). Auth is required iff a
non-empty password is configured (`auth.ts:28-30`). `opencode serve` always has one: from
`OPENCODE_PASSWORD` / legacy `OPENCODE_SERVER_PASSWORD`, else (service mode) the service config
file, else a random 32-byte base64url string printed as `server password <pw>` in foreground mode
(`packages/cli/src/server-process.ts:78-90,196`; `packages/cli/src/env.ts`).
`ServerProcess.start` refuses to start without one (`process.ts:53-54`).

A request is authorized if ANY of (`packages/server/src/middleware/authorization.ts:32-71`):

1. **Query `auth_token`** — value is base64(`username:password`) (same payload as Basic). Takes
   precedence over the header if present (authorization.ts:34-35).
2. **`Authorization: Basic <base64(user:pass)>`** (case-insensitive scheme, regex
   `^Basic\s+(.+)$`). Decode failure or missing `:` ⇒ empty credentials.
   Password matches if it equals the server password **or** is a valid session token (4.2)
   (`auth.ts:33-37`).
3. **Session cookie** `opencode_session_<port>` (or `opencode_session` if Host has no port)
   (`auth.ts:58-61`), valid token, AND (`Origin` header absent OR `URL(origin).host == Host`)
   (authorization.ts:64-71).

There is no Bearer scheme.

### 4.2 Session tokens (pairing)

`token = "<expiresUnixSeconds>.<sig>"`, `expires = floor(now/1000) + 2592000` (30 days,
`auth.ts:16,40-44`). `sig = base64url(HMAC_SHA256(key, expiresString))` with
`key = HMAC_SHA256(key=password, msg="opencode-session-v1")` (raw 32 bytes) (`auth.ts:63-66`).
Verify: exactly two `.`-parts, expires is a safe integer and `expires*1000 > now`,
constant-time compare of the base64url strings (`auth.ts:46-55`). Rotating the password
invalidates all tokens.

Pairing flow (`packages/server/src/handlers/server.ts:29-53`, `pairing.ts`):
`POST /api/pair` (authenticated) → `{code, expires_in:300}`; code = 16 random bytes base64url,
single-use, 5 min TTL, in-memory cache capacity 1000 (`pairing.ts:7,22-31`).
`GET /auth/connect/{code}` is auth-exempt (path regex `^/auth/connect/[^/]+$`,
`packages/protocol/src/groups/server.ts:33-38`) — see §7 #3 for the browser/JSON split.

### 4.3 Unauthorized response

`401` body `{"_tag":"UnauthorizedError","message":"Authentication required"}`, header
`www-authenticate: Basic realm="Secure Area"` **only if** the request's `sec-fetch-mode` header
is absent or equals `navigate` (so browser `fetch` never triggers a native prompt)
(authorization.ts:41-56,86-91).

### 4.4 Auth-exempt routes

* `GET /auth/connect/{code}`.
* `/api/pty/{id}/connect` and `/api/experimental/persistent-pty/{id}/connect` **when a non-empty
  `ticket` query param is present** (the handler validates the ticket; regexes in
  `protocol/src/groups/pty.ts:13-19`, `persistent-pty.ts:12-16`). In the process dispatcher this
  exemption applies only after the app is ready (process.ts:192-197).
* Everything served by the web-UI transform (non-API paths), which runs before `dispatch`.
* `OPTIONS` (CORS preflight; answered by the CORS middleware before auth).

### 4.5 Two auth layers

`dispatch` (process.ts:192-197) authenticates every API request before routing, so even unknown
`/api/*` paths and `/openapi.json` return 401 when unauthenticated (process test
`packages/server/test/process.test.ts:126-136`). The HttpApi `Authorization` middleware
(authorization.ts:73-95) repeats the check for embedded hosts. Unknown routes after auth →
404 with empty body (web-ui transform converts `RouteNotFound` to `404`, web-ui.ts:26-28).

---

## 5. CORS

`HttpMiddleware.cors({allowedOrigins: isAllowedCorsOrigin, maxAge: 86400})`
(process.ts:74, fetch.ts:51). Allowed origins (`packages/server/src/cors.ts:3-20`):

* no `Origin` header;
* prefix `http://localhost:` or `http://127.0.0.1:` (any port; note: `http://localhost` without
  port is NOT allowed);
* prefix `oc://renderer`;
* exactly `tauri://localhost`, `http://tauri.localhost`, `https://tauri.localhost`;
* regex `^https://([a-z0-9-]+\.)*opencode\.ai$`;
* any exact string in `options.cors` (CLI `--cors`, or service config `cors`).

Behaviour (Effect cors middleware, predicate mode):

* `OPTIONS` (any path, before auth/readiness): `204`, headers
  `access-control-allow-origin: <origin>` + `vary: Origin` (only if allowed),
  `access-control-allow-methods: GET, HEAD, PUT, PATCH, POST, DELETE`,
  `access-control-allow-headers: <echo of access-control-request-headers>` +
  `vary: Access-Control-Request-Headers` (if the request sent that header),
  `access-control-max-age: 86400`. No `allow-credentials`.
* Other requests: add `access-control-allow-origin: <origin>` + `vary: Origin` when allowed
  (including on 401s). Disallowed origins are not blocked server-side (browser enforces), except:
* **PTY**: `pty.connect.token`, `persistentPty.connectToken`, `pty.connect`,
  `persistentPty.connect` additionally require `isAllowedRequestOrigin` = allowed CORS origin OR
  `URL(origin).host == Host` (cors.ts:22-34); otherwise 403.

---

## 6. Error envelope

### 6.1 Typed errors

Every declared error is a JSON object with a `_tag` discriminator plus the fields below, served
with the listed status and `content-type: application/json`
(`packages/protocol/src/errors.ts`). Optional fields may be absent or `null` (§2.1).

| `_tag` | Status | Fields (besides `_tag`) | errors.ts |
|---|---|---|---|
| `InvalidRequestError` | 400 | `message`, `kind?`, `field?` | :11 |
| `InvalidCursorError` | 400 | `message` | :203 |
| `RpcError` | 400 | `type`, `message`, `data?` | :21 |
| `FormInvalidAnswerError` | 400 | `id`, `message` | :236 |
| `WorktreeError` | 400 | `name:"WorktreeError"`, `data:{message, forceRequired?}` | groups/worktree.ts:9-19 |
| `UnauthorizedError` | 401 | `message` | :41 |
| `ForbiddenError` | 403 | `message` | :245 |
| `LocationNotFoundError` | 404 | `location:{directory}`, `message` | :5 |
| `SessionNotFoundError` | 404 | `sessionID`, `message` | :148 |
| `MessageNotFoundError` | 404 | `sessionID`, `messageID`, `message` | :157 |
| `AgentNotFoundError` | 404 | `agentID`, `message` | :139 |
| `ProviderNotFoundError` | 404 | `providerID`, `message` | :83 |
| `IntegrationNotFoundError` | 404 | `integrationID`, `message` | :92 |
| `IntegrationAttemptNotFoundError` | 404 | `integrationID`, `attemptID`, `message` | :101 |
| `IntegrationMethodNotFoundError` | 404 | `integrationID`, `methodID`, `message` | :111 |
| `ProjectNotFoundError` | 404 | `projectID`, `message` | :121 |
| `FileNotFoundError` | 404 | `path`, `message` | :130 |
| `SkillNotFoundError` | 404 | `skill`, `message` | :167 |
| `McpServerNotFoundError` | 404 | `server`, `message` | :176 |
| `CommandNotFoundError` | 404 | `command`, `message` | :185 |
| `PermissionNotFoundError` | 404 | `requestID`, `message` | :209 |
| `FormNotFoundError` | 404 | `id`, `message` | :218 |
| `PtyNotFoundError` | 404 | `ptyID`, `message` | :251 |
| `ShellNotFoundError` | 404 | `id`, `message` | :260 |
| `ConflictError` | 409 | `message`, `resource?` | :47 |
| `SessionBusyError` | 409 | `sessionID`, `message` | :56 |
| `FormAlreadySettledError` | 409 | `id`, `message` | :227 |
| `InstructionEntryValueTooLargeError` | 413 | `actualBytes`, `maxBytes`, `message` | packages/schema/src/instruction-entry.ts:33 |
| `UnknownError` | 500 | `message`, `ref?` | :74 |
| `CommandExecutionError` | 500 | `command`, `message` | :194 |
| `RpcInternalError` | 500 | `type:"rpc.internal"\|"rpc.invalid_output"`, `message`, `data?` | :31 |
| `VcsInitNotSupportedError` | 501 | `providerID`, `message` | groups/vcs.ts:15-19 |
| `ServiceUnavailableError` | 503 | `message`, `service?` | :65 |

When an endpoint declares several errors with the same status the OpenAPI uses `anyOf`; the
client distinguishes by `_tag` (and for `WorktreeError` also `name`/`data.message`,
client.ts:2286-2297). Messages are human strings; where §7 quotes an exact template
(e.g. `Session not found: <id>`, `packages/server/src/handlers/session-error.ts:6-11`) it should
be reproduced verbatim.

`UnknownError.ref` is `"err_" + first 8 chars of a random UUID`; message is always
`"Unexpected server error. Check server logs for details."` (session-error.ts:21-42).

### 6.2 Schema (decode) errors

Any request decode failure (path params, headers, query, body) is converted by
`schemaErrorLayer` (`packages/server/src/middleware/schema-error.ts:14-20`) to
`400 {"_tag":"InvalidRequestError","message":<reason>,"kind":<"Params"|"Headers"|"Query"|"Body"|"Payload">}`.
`reason` = Effect's `SchemaError.message`, truncated to 1024 chars + `"... (<n> more chars)"`.
The exact Effect message text is not reproducible byte-for-byte; clients only key on `_tag`
(see Q4).

### 6.3 Untyped failures

* Defects / unexpected exceptions → `500` with empty body (Effect `causeResponse`).
* Interrupted by client → 499; server abort → 503 (Effect defaults; rarely observable).
* Unknown route → `404` empty body.
* Readiness: `503` JSON `{code:"service_starting"|"service_stopping"}` with `retry-after: 1`,
  or `503 {code:"service_failed", message:"The background service could not start.",
  action:"Run `opencode service restart` after checking the service logs."}` (process.ts:219-233).
  Note these bodies have no `_tag`.

---

## 7. Endpoint catalog

Legend: **Scope** per §1.1 (`G*` = special-cased by the dispatcher before readiness). **OK** =
success status. **Core call** = the core service method the handler invokes (service names are
the TS `Context.Service` modules under `packages/core/src`). "internal" = the endpoint name used in
protocol/handler code (differs from `operationId` for some). Every endpoint additionally may
return `401 UnauthorizedError` and `400 InvalidRequestError` (schema errors); these appear in the
OpenAPI error list and are repeated below. Types use OpenAPI component names
(`#/components/schemas/<Name>`), `X|null` = nullable/optional-with-null, `?` = optional key.

### 7.0 Key shapes (JSON)

```jsonc
// ServerInfo (GET /api/info)
{"version":"1.2.3","pid":1234,"urls":["http://127.0.0.1:4096"],"paths":{"tmp":"/tmp/..."},
 "capabilities":{"persistentPty":true}}           // capabilities absent before ready
// PairingCode / PairingSession
{"code":"<22 chars base64url>","expires_in":300}   {"token":"1767225600.<base64url sig>"}
// Location envelope (L endpoints)
{"location":{"directory":"/home/u/proj"},"data":[ /* T */ ]}
// Location.PublicInfo (GET /api/location)
{"directory":"/home/u/proj","project":{"id":"<Project.ID>","directory":"/home/u/proj","canonical":"/home/u/proj"}}
// SessionsResponse / SessionMessagesResponse
{"data":[/* Session.Info | Session.Message.Info */],"cursor":{"previous":"<b64url>|null","next":"<b64url>|null"}}
// POST /api/session/{id}/prompt body
{"id":"msg_…"|null, "text":"hello", "files":[{"uri":"file:///…","name":"…","description":"…","mention":{…}}],
 "agents":[{"name":"build","mention":{…}}], "skills":[{"id":"<Skill.ID>","mention":{…}}],
 "metadata":{}, "delivery":"steer"|"queue"|null, "resume":true|false|null}
// Model.Ref
{"id":"claude-…","providerID":"anthropic","variant":"…"}
// Rpc.Input / Rpc.Output
{"input":<any>}   {"output":<any>}  // output key omitted when undefined
// PtyTicket.ConnectToken (wrapped: {location,data} for /api/pty, {data} for persistent)
{"ticket":"<uuid v4>","expires_in":60}
// Pty
{"id":"pty_…","title":"…","command":"/bin/zsh","args":[],"cwd":"/…","status":"running"|"exited","pid":1,"exitCode":0}
// Error envelope
{"_tag":"SessionNotFoundError","sessionID":"ses_…","message":"Session not found: ses_…"}
{"_tag":"InvalidRequestError","message":"…","kind":"Payload","field":null}
```

### 7.1 `server`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 1 | GET | `/api/info` | `server.info` | G* | 200 | `ServerInfo.Service` |
| 2 | POST | `/api/pair` | `server.pair` | G | 200 | `ServerPairing.issue()` |
| 3 | GET | `/auth/connect/{code}` | `server.connect` | G (auth-exempt) | 200 | `ServerPairing.consume(code) + ServerAuth.issueSession` |

**1. `GET /api/info`** (`server.info`; internal `server.info`) — Get server info.
  _Return the server identity, connection URLs, paths, and readiness status._
  - Response: `200 ServerInfo`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Returns `{version: app.version ?? "unknown", pid: process.pid ?? 0, urls: urls(), paths:{tmp}, capabilities:{persistentPty: PersistentPty.available}}`. Before the app is ready, the process dispatcher answers this route itself (see §10.3): body `{version,pid,urls,paths}` (no `capabilities`) with status 503 + `retry-after: 1` (starting/stopping) or 500 (failed).
  - Src: `packages/protocol/src/groups/server.ts:42`, `packages/server/src/handlers/server.ts:17`

**2. `POST /api/pair`** (`server.pair`; internal `server.pair`) — Create pairing code.
  _Create a short-lived, single-use code for a /auth/connect/:code pairing link._
  - Response: `200 PairingCode`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Code = 16 random bytes, base64url (22 chars). `expires_in` = 300. Single use, in-memory cache (capacity 1000).
  - Src: `packages/protocol/src/groups/server.ts:53`, `packages/server/src/handlers/server.ts:29`

**3. `GET /auth/connect/{code}`** (`server.connect`; internal `server.connect`) — Redeem pairing code.
  _Redeem a pairing code. Browsers receive a session cookie and a redirect to the web app; requests that accept JSON receive a session token to use as the password._
  - Params: p `code`: `string`
  - Response: `200 PairingSession`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: `Accept` contains `text/html` => browser: success = `302 Location: /` + `Set-Cookie: opencode_session_<port>=<token>; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000`; failure = `401 text/plain` "This pairing link expired or was already used. Run `opencode pair` to get a new one.". Otherwise: success `200 {token}`, failure `401 {_tag:"UnauthorizedError",message:"Pairing link expired or already used"}`.
  - Src: `packages/protocol/src/groups/server.ts:64`, `packages/server/src/handlers/server.ts:30`


### 7.2 `location`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 4 | GET | `/api/location` | `location.get` | L | 200 | `Location.Service` |
| 5 | POST | `/api/location/reload` | `location.reload` | G | 204 | `LocationServiceMap.reload()` |

**4. `GET /api/location`** (`location.get`; internal `location.get`) — Get location.
  _Resolve the requested location or the server default location._
  - Params: q `location[directory]`?
  - Response: `200 Location.PublicInfo`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Returns full `Location.PublicInfo` (directory + project{id,directory,canonical}).
  - Src: `packages/protocol/src/groups/location.ts:36`, `packages/server/src/handlers/location.ts:12`

**5. `POST /api/location/reload`** (`location.reload`; internal `location.reload`) — Reload configuration.
  _Shut down and rebuild every loaded location. Pending permissions and forms are cancelled; running sessions continue with fresh services at the next step boundary. Emits location.shutdown for client recovery and responds once all replacement builds settle._
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Behavior: Invalidates every loaded location then rebuilds all concurrently; any failure => 503 `{service:"location", message: Cause.pretty(cause)}`.
  - Src: `packages/protocol/src/groups/location.ts:51`, `packages/server/src/handlers/location.ts:22`


### 7.3 `agent`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 6 | GET | `/api/agent` | `agent.list` | L | 200 | `Agent.list()` |
| 7 | GET | `/api/agent/{agentID}` | `agent.get` | L | 200 | `Agent.get(agentID)` |

**6. `GET /api/agent`** (`agent.list`; internal `agent.list`) — List agents.
  _Retrieve currently registered agents._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Agent.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/agent.ts:10`, `packages/server/src/handlers/agent.ts:10`

**7. `GET /api/agent/{agentID}`** (`agent.get`; internal `agent.get`) — Get agent.
  _Retrieve a single currently registered agent._
  - Params: p `agentID`: `Agent.ID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Agent.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 AgentNotFoundError/LocationNotFoundError
  - Behavior: Missing => 404 `AgentNotFoundError` message `Agent not found: <id>`.
  - Src: `packages/protocol/src/groups/agent.ts:24`, `packages/server/src/handlers/agent.ts:15`


### 7.4 `plugin`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 8 | GET | `/api/plugin` | `plugin.list` | L | 200 | `Plugin.list()` |
| 9 | POST | `/api/plugin/check` | `plugin.check` | L | 200 | `Plugin.list() + PluginUpdate.check(target,{refresh:true})` |
| 10 | POST | `/api/plugin/update` | `plugin.update` | L | 204 | `PluginUpdate.update(target) x N (concurrent, all run to completion)` |

**8. `GET /api/plugin`** (`plugin.list`; internal `plugin.list`) — List plugins.
  _Retrieve enabled server plugins and their current status._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Plugin.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/plugin.ts:10`, `packages/server/src/handlers/plugin.ts:11`

**9. `POST /api/plugin/check`** (`plugin.check`; internal `plugin.check`) — Check plugin updates.
  _Check one or all package plugins for available updates._
  - Params: q `location[directory]`?
  - Body: `{target?: string|null}`
  - Response: `200 {location: Location.PublicRef, data: Plugin.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Targets = distinct `source.target` of `package` plugins, filtered to `body.target` if given. Unknown target => 400 `{field:"target"}`. Returns full inventory; package plugins found outdated get `source.outdated: true`.
  - Src: `packages/protocol/src/groups/plugin.ts:24`, `packages/server/src/handlers/plugin.ts:16`

**10. `POST /api/plugin/update`** (`plugin.update`; internal `plugin.update`) — Update plugins.
  _Update package plugins concurrently and notify active locations to reload them. Responds once every update has finished; fails when any update fails._
  - Params: q `location[directory]`?
  - Body: `{targets: string[]}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Unknown targets => 400 `{field:"targets", message:"Plugin packages are not in the current server inventory: a, b"}`. Any failure => 503 `{service:"plugin", message:"Failed to update plugin packages: <t>: <cause>; ..."}`.
  - Src: `packages/protocol/src/groups/plugin.ts:40`, `packages/server/src/handlers/plugin.ts:55`


### 7.5 `session`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 11 | GET | `/api/session` | `session.list` | G | 200 | `Session.list({...query, limit})` |
| 12 | POST | `/api/session` | `session.create` | G | 200 | `Session.create(...)` |
| 13 | GET | `/api/experimental/session/stats` | `experimental.session.stats` | G | 200 | `SessionStats.get({from,to,projectID,timezone,tools})` |
| 14 | POST | `/api/experimental/session/import` | `experimental.session.import` | G | 200 | `SessionTransfer.import({data:{info,messages}, location: body.location ?? cwd})` |
| 15 | GET | `/api/experimental/session/{sessionID}/export` | `experimental.session.export` | G | 200 | `SessionTransfer.export({sessionID, sanitize})` |
| 16 | GET | `/api/session/active` | `session.active` | G | 200 | `Session.active` |
| 17 | GET | `/api/session/{sessionID}` | `session.get` | G | 200 | `Session.get(id)` |
| 18 | DELETE | `/api/session/{sessionID}` | `session.remove` | G | 204 | `Session.remove(id)` |
| 19 | PATCH | `/api/session/{sessionID}` | `session.update` | S | 204 | `Session.get; rename / SessionTitle.generate; setMetadata; setPermissions` |
| 20 | POST | `/api/session/{sessionID}/fork` | `session.fork` | S | 200 | `Session.fork({sessionID, before})` |
| 21 | POST | `/api/session/{sessionID}/agent` | `session.switchAgent` | S | 204 | `Session.switchAgent` |
| 22 | POST | `/api/session/{sessionID}/model` | `session.switchModel` | S | 204 | `Session.switchModel` |
| 23 | POST | `/api/session/{sessionID}/move` | `session.move` | G | 204 | `Session.move({sessionID, directory, delivery})` |
| 24 | POST | `/api/session/{sessionID}/prompt` | `session.prompt` | S | 200 | `Session.prompt({...})` |
| 25 | POST | `/api/session/{sessionID}/command` | `session.command` | S | 204 | `Session.command({command: body.name, ...})` |
| 26 | POST | `/api/experimental/session/{sessionID}/skill` | `experimental.session.skill` | S | 204 | `Session.skill({sessionID, skill: body.id, resume})` |
| 27 | POST | `/api/session/{sessionID}/synthetic` | `session.synthetic` | S | 200 | `Session.synthetic(...)` |
| 28 | POST | `/api/session/{sessionID}/shell` | `session.shell` | S | 204 | `Session.shell({sessionID, id, command})` |
| 29 | POST | `/api/session/{sessionID}/compact` | `session.compact` | S | 200 | `Session.compact({sessionID, id, delivery})` |
| 30 | POST | `/api/experimental/session/{sessionID}/wait` | `experimental.session.wait` | S | 204 | `Session.wait(id)` |
| 31 | POST | `/api/session/{sessionID}/revert/stage` | `session.revert.stage` | S | 200 | `Session.revert.stage({sessionID, messageID, files})` |
| 32 | DELETE | `/api/session/{sessionID}/revert` | `session.revert.clear` | S | 204 | `Session.revert.clear(id)` |
| 33 | POST | `/api/session/{sessionID}/revert/commit` | `session.revert.commit` | S | 204 | `Session.revert.commit(id)` |
| 34 | GET | `/api/session/{sessionID}/context` | `session.context` | G | 200 | `Session.context(id)` |
| 35 | GET | `/api/session/{sessionID}/diff` | `session.diff` | G | 200 | `Session.diff({sessionID, from, to, context})` |
| 36 | GET | `/api/session/{sessionID}/inbox` | `session.inbox.list` | G | 200 | `Session.inbox(id)` |
| 37 | DELETE | `/api/session/{sessionID}/inbox/{inboxID}` | `session.inbox.cancel` | G | 204 | `Session.cancelInbox` |
| 38 | PATCH | `/api/session/{sessionID}/inbox/{inboxID}` | `session.inbox.update` | G | 204 | `Session.steerInbox / Session.queueInbox` |
| 39 | GET | `/api/experimental/session/{sessionID}/instructions/entries` | `experimental.session.instructions.entry.list` | S | 200 | `InstructionEntry.list(sessionID)` |
| 40 | PUT | `/api/experimental/session/{sessionID}/instructions/entries/{key}` | `experimental.session.instructions.entry.put` | S | 204 | `InstructionEntry.put({sessionID,key,value})` |
| 41 | DELETE | `/api/experimental/session/{sessionID}/instructions/entries/{key}` | `experimental.session.instructions.entry.remove` | S | 204 | `InstructionEntry.remove({sessionID,key})` |
| 42 | POST | `/api/session/{sessionID}/generate` | `session.generate` | S | 200 | `Session.generate({sessionID, prompt})` |
| 43 | GET | `/api/experimental/session/{sessionID}/log` | `session.log` | G | 200 | `Session.get + Session.log({sessionID, after, follow})` |
| 44 | POST | `/api/session/{sessionID}/interrupt` | `session.interrupt` | S | 200 | `Session.interrupt(id, {resume})` |
| 45 | POST | `/api/session/{sessionID}/background` | `session.background` | S | 204 | `Session.background(id)` |
| 46 | GET | `/api/session/{sessionID}/message/{messageID}` | `session.message.get` | G | 200 | `Session.get + Session.message({sessionID,messageID})` |
| 47 | GET | `/api/session/{sessionID}/form` | `session.form.list` | F | 200 | `Form.list({sessionID})` |
| 48 | POST | `/api/session/{sessionID}/form` | `session.form.create` | F | 200 | `Form.create({id,sessionID,title,metadata,fields})` |
| 49 | GET | `/api/session/{sessionID}/form/{formID}` | `session.form.get` | F | 200 | `Form.get + Form.state` |
| 50 | DELETE | `/api/session/{sessionID}/form/{formID}` | `session.form.cancel` | F | 204 | `Form.cancel(formID, {message})` |
| 51 | POST | `/api/session/{sessionID}/form/{formID}/reply` | `session.form.reply` | F | 204 | `Form.reply({id, answer})` |
| 52 | PUT | `/api/session/{sessionID}/environment` | `session.environment` | G | 204 | `Session.environment({sessionID, variables})` |
| 53 | POST | `/api/session/{sessionID}/view` | `session.view` | G | 204 | `Session.view({sessionID, idle})` |
| 54 | GET | `/api/session/{sessionID}/message` | `session.message.list` | G | 200 | `Session.messages({sessionID, limit, order, type, cursor})` |

**11. `GET /api/session`** (`session.list`; internal `session.list`) — List sessions.
  _Retrieve sessions in the requested order. Items keep that order across pages; use cursor.next or cursor.previous to move through the ordered list._
  - Params: q `limit`?: `string|null`; q `order`?: `"asc"|"desc"|null`; q `search`?: `string|null`; q `parentID`?: `(Session.ID|"null")|null`; q `directory`?: `AbsolutePath|null`; q `project`?: `Project.ID|null`; q `subpath`?: `RelativePath|null`; q `cursor`?: `SessionsCursor|null`
  - Response: `200 SessionsResponse`
  - Errors: 400 InvalidCursorError/InvalidRequestError; 401 UnauthorizedError
  - Behavior: `limit` default 50. When `cursor` is present the other query params are ignored and the cursor's embedded query is used (§2.5). Response always has `cursor` object; `previous`/`next` are `null` when the page is empty.
  - Src: `packages/protocol/src/groups/session.ts:182`, `packages/server/src/handlers/session.ts:61`

**12. `POST /api/session`** (`session.create`; internal `session.create`) — Create session.
  _Create a session at the requested location. A parentID creates a linked child session at its parent's location._
  - Body: `{id?: Session.ID|null, parentID?: Session.ID|null, title?: string|null, agent?: Agent.ID|null, model?: Model.Ref|null, location?: Location.PublicRef|null, metadata?: Session.Metadata|null, permissions?: Permission.Ruleset|null}`
  - Response: `200 {data: Session.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: Without `parentID`: location = `body.location ?? {directory: process.cwd()}`. With `parentID`: body.location is ignored, child inherits parent's location; missing parent => 404 SessionNotFoundError.
  - Src: `packages/protocol/src/groups/session.ts:221`, `packages/server/src/handlers/session.ts:125`

**13. `GET /api/experimental/session/stats`** (`experimental.session.stats`; internal `session.stats`) — Get session statistics.
  _Aggregate local session activity, usage, and tool reliability for a time range._
  - Params: q `from`?: `string|null`; q `to`?: `string|null`; q `project`?: `Project.ID|null`; q `timezone`?: `string|null`; q `tools`?: `"none"|"summary"|"detail"|null`
  - Response: `200 {data: SessionStats.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: `timezone` default `UTC`; invalid => 400 `Invalid time zone: <tz>`. Any stats error => 400 `Stats range must end after it starts`.
  - Src: `packages/protocol/src/groups/session.ts:202`, `packages/server/src/handlers/session.ts:104`

**14. `POST /api/experimental/session/import`** (`experimental.session.import`; internal `session.import`) — Import session.
  _Import a projected session transcript at the requested location. If parentID is supplied, the parent session must already exist; import parents before children._
  - Body: `{info: Session.Info, messages: Session.Message.Info[], location?: Location.PublicRef|null}`
  - Response: `200 {data: Session.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError; 409 ConflictError
  - Behavior: Existing session ID => 409 `{message:"Session already exists: <id>", resource:<id>}`.
  - Src: `packages/protocol/src/groups/session.ts:244`, `packages/server/src/handlers/session.ts:145`

**15. `GET /api/experimental/session/{sessionID}/export`** (`experimental.session.export`; internal `session.export`) — Export session.
  _Export a complete projected session transcript._
  - Params: p `sessionID`: `Session.ID`; q `sanitize`?: `"true"|"false"|null`
  - Response: `200 {data: SessionTransfer.Data}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError; 500 UnknownError
  - Behavior: Message decode failure => 500 `UnknownError{message:"Unexpected server error. Check server logs for details.", ref:"err_xxxxxxxx"}`.
  - Src: `packages/protocol/src/groups/session.ts:261`, `packages/server/src/handlers/session.ts:168`

**16. `GET /api/session/active`** (`session.active`; internal `session.active`) — List active sessions.
  _Retrieve foreground Session drains currently owned by this OpenCode process. Sessions absent from the result are inactive._
  - Response: `200 {data: object}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: `data` is `{[sessionID]: {type:"running"}}` for every foreground drain owned by this process.
  - Src: `packages/protocol/src/groups/session.ts:275`, `packages/server/src/handlers/session.ts:181`

**17. `GET /api/session/{sessionID}`** (`session.get`; internal `session.get`) — Get session.
  _Retrieve a session by ID._
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: Session.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:287`, `packages/server/src/handlers/session.ts:190`

**18. `DELETE /api/session/{sessionID}`** (`session.remove`; internal `session.remove`) — Delete session.
  _Delete a session and its child sessions._
  - Params: p `sessionID`: `Session.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: Deletes children too.
  - Src: `packages/protocol/src/groups/session.ts:300`, `packages/server/src/handlers/session.ts:209`

**19. `PATCH /api/session/{sessionID}`** (`session.update`; internal `session.update`) — Update session.
  _Update mutable session properties._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{title?: string|null, metadata?: Session.Metadata|null, permissions?: Permission.Ruleset|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: Applied in order title, metadata, permissions; each only if key present. `title: ""` (empty) triggers AI title generation instead of rename.
  - Src: `packages/protocol/src/groups/session.ts:362`, `packages/server/src/handlers/session.ts:258`

**20. `POST /api/session/{sessionID}/fork`** (`session.fork`; internal `session.fork`) — Fork session.
  _Create a child session by copying projected history before a message. Omit before to copy the full history._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{before?: Session.Message.ID|null}`
  - Response: `200 {data: Session.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/MessageNotFoundError/LocationNotFoundError
  - Behavior: `Session.ForkEmptyError` => 400 `{kind:"empty_session"}`.
  - Src: `packages/protocol/src/groups/session.ts:313`, `packages/server/src/handlers/session.ts:225`

**21. `POST /api/session/{sessionID}/agent`** (`session.switchAgent`; internal `session.switchAgent`) — Switch session agent.
  _Switch the agent used by subsequent provider turns._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{agent: Agent.ID}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:330`, `packages/server/src/handlers/session.ts:240`

**22. `POST /api/session/{sessionID}/model`** (`session.switchModel`; internal `session.switchModel`) — Switch session model.
  _Switch the model used by subsequent provider turns._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{model: Model.Ref}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:346`, `packages/server/src/handlers/session.ts:249`

**23. `POST /api/session/{sessionID}/move`** (`session.move`; internal `session.move`) — Move session.
  _Move a session to another project directory at the requested delivery boundary._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{directory: AbsolutePath, delivery?: Session.Inbox.Delivery|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: Destination errors => 400 messages `Directory does not exist: <d>` / `Not a directory: <d>` / `Directory is unavailable: <d>`.
  - Src: `packages/protocol/src/groups/session.ts:382`, `packages/server/src/handlers/session.ts:283`

**24. `POST /api/session/{sessionID}/prompt`** (`session.prompt`; internal `session.prompt`) — Send message.
  _Durably admit one session input and schedule agent-loop execution unless resume is false._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id?: Session.Message.ID|null, text: string, files?: PromptInput.FileAttachment[], agents?: Prompt.AgentAttachment[], skills?: PromptInput.SkillAttachment[], metadata?: object, delivery?: Session.Inbox.Delivery|null, resume?: boolean|null}`
  - Response: `200 {data: Session.Inbox.User}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 ConflictError
  - Behavior: 409 on ID conflict (`Prompt message ID conflicts with an existing durable record: <id>`); attachment error => 400 `{field:"files"}`; unknown skill => 400 `{field:"skills", message:"Skill not found: <s>"}`; DirectoryNotFound => 404 LocationNotFoundError.
  - Src: `packages/protocol/src/groups/session.ts:396`, `packages/server/src/handlers/session.ts:307`

**25. `POST /api/session/{sessionID}/command`** (`session.command`; internal `session.command`) — Run command.
  _Execute a slash command callback immediately._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{name: string, text: string, files?: PromptInput.FileAttachment[], agents?: Prompt.AgentAttachment[], skills?: PromptInput.SkillAttachment[], delivery?: Session.Inbox.Delivery|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/CommandNotFoundError/LocationNotFoundError; 500 CommandExecutionError
  - Behavior: `Command.NotFoundError` => 404 CommandNotFoundError; `Command.ExecutionError` => 500 CommandExecutionError.
  - Src: `packages/protocol/src/groups/session.ts:418`, `packages/server/src/handlers/session.ts:344`

**26. `POST /api/experimental/session/{sessionID}/skill`** (`experimental.session.skill`; internal `session.skill`) — Activate skill.
  _Activate a skill for a session by appending a skill message and resuming execution._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id: Skill.ID, resume?: boolean|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/SkillNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:438`, `packages/server/src/handlers/session.ts:380`

**27. `POST /api/session/{sessionID}/synthetic`** (`session.synthetic`; internal `session.synthetic`) — Add synthetic message.
  _Durably admit synthetic session input and schedule execution unless resume is false._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id?: Session.Message.ID|null, text: string, description?: string|null, metadata?: object, delivery?: Session.Inbox.Delivery|null, resume?: boolean|null}`
  - Response: `200 {data: Session.Inbox.Synthetic}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 ConflictError
  - Behavior: 409 `Synthetic input ID conflicts with an existing durable record: <id>`.
  - Src: `packages/protocol/src/groups/session.ts:457`, `packages/server/src/handlers/session.ts:399`

**28. `POST /api/session/{sessionID}/shell`** (`session.shell`; internal `session.shell`) — Run shell command.
  _Execute one shell command in the session's working directory. Emits a shell.started event before execution and a shell.ended event with the merged output after._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id?: Session.Message.ID|null, command: string}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: Responds 204 once Session.shell returns (core emits session.shell.started / session.shell.ended).
  - Src: `packages/protocol/src/groups/session.ts:480`, `packages/server/src/handlers/session.ts:426`

**29. `POST /api/session/{sessionID}/compact`** (`session.compact`; internal `session.compact`) — Compact session.
  _Durably admit a session compaction request. Steers by default: it runs at the next step boundary instead of waiting behind queued prompts._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id?: Session.Message.ID|null, delivery?: Session.Inbox.Delivery|null}`
  - Response: `200 {data: Session.Inbox.Compaction}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 ConflictError
  - Behavior: 409 `Compaction input ID conflicts with an existing durable record: <id>`.
  - Src: `packages/protocol/src/groups/session.ts:500`, `packages/server/src/handlers/session.ts:435`

**30. `POST /api/experimental/session/{sessionID}/wait`** (`experimental.session.wait`; internal `session.wait`) — Wait for session.
  _Wait for a session agent loop to become idle._
  - Params: p `sessionID`: `Session.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Long-poll: responds 204 when the agent loop is idle.
  - Src: `packages/protocol/src/groups/session.ts:520`, `packages/server/src/handlers/session.ts:455`

**31. `POST /api/session/{sessionID}/revert/stage`** (`session.revert.stage`; internal `session.revert.stage`) — Stage session revert.
  _Stage or move a reversible session boundary and optionally apply its file changes._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{messageID: Session.Message.ID, files?: boolean|null}`
  - Response: `200 {data: Session.Revert}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 MessageNotFoundError/SessionNotFoundError/LocationNotFoundError; 409 SessionBusyError; 500 UnknownError
  - Behavior: `Session.BusyError` => 409 SessionBusyError `Session is busy: <id>`; `Snapshot.Error` => 500 UnknownError with `ref`.
  - Src: `packages/protocol/src/groups/session.ts:535`, `packages/server/src/handlers/session.ts:462`

**32. `DELETE /api/session/{sessionID}/revert`** (`session.revert.clear`; internal `session.revert.clear`) — Clear staged revert.
  - Params: p `sessionID`: `Session.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 SessionBusyError; 500 UnknownError
  - Behavior: Same error mapping as stage.
  - Src: `packages/protocol/src/groups/session.ts:551`, `packages/server/src/handlers/session.ts:483`

**33. `POST /api/session/{sessionID}/revert/commit`** (`session.revert.commit`; internal `session.revert.commit`) — Commit staged revert.
  - Params: p `sessionID`: `Session.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 SessionBusyError
  - Behavior: Busy => 409.
  - Src: `packages/protocol/src/groups/session.ts:560`, `packages/server/src/handlers/session.ts:498`

**34. `GET /api/session/{sessionID}/context`** (`session.context`; internal `session.context`) — Get session context.
  _Retrieve the active context messages for a session (all messages after the last compaction)._
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: Session.Message.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError; 500 UnknownError
  - Behavior: Decode failure => 500 UnknownError+ref.
  - Src: `packages/protocol/src/groups/session.ts:569`, `packages/server/src/handlers/session.ts:511`

**35. `GET /api/session/{sessionID}/diff`** (`session.diff`; internal `session.diff`) — Diff session turns.
  _Structured per-file diffs of the files a turn changed. A turn runs from the first prompt after the session was last idle until its next idle marker, so prompts steered in while it was busy belong to the same turn; `to` extends the range through a later turn. Compares the range's first recorded snapshot with its last; a step still running in the active session compares against the working copy. Ranges that span a location change are rejected. In sessions without any idle marker, a prompt's turn spans until the next user message._
  - Params: p `sessionID`: `Session.ID`; q `from`?: `Session.Message.ID|null`; q `to`?: `Session.Message.ID|null`; q `context`?: `string|null`
  - Response: `200 {data: FileDiff.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 MessageNotFoundError/SessionNotFoundError/LocationNotFoundError; 500 UnknownError
  - Behavior: `Session.TurnRangeError` => 400 `{field}`; Snapshot error => 500 UnknownError+ref.
  - Src: `packages/protocol/src/groups/session.ts:582`, `packages/server/src/handlers/session.ts:524`

**36. `GET /api/session/{sessionID}/inbox`** (`session.inbox.list`; internal `session.inbox.list`) — List session inbox.
  _List durable enqueued session work not yet delivered, ordered by enqueue sequence. Includes user, synthetic, compaction, and move items._
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: Session.Inbox.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:607`, `packages/server/src/handlers/session.ts:541`

**37. `DELETE /api/session/{sessionID}/inbox/{inboxID}`** (`session.inbox.cancel`; internal `session.inbox.cancel`) — Cancel inbox input.
  _Cancel an inbox item that has not yet been delivered. Unavailable items are a no-op._
  - Params: p `sessionID`: `Session.ID`; p `inboxID`: `Session.Message.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: Inbox conflict is swallowed => always 204 if session exists.
  - Src: `packages/protocol/src/groups/session.ts:621`, `packages/server/src/handlers/session.ts:551`

**38. `PATCH /api/session/{sessionID}/inbox/{inboxID}`** (`session.inbox.update`; internal `session.inbox.update`) — Update inbox item.
  _Change a pending inbox item's delivery mode. Steering wakes session execution._
  - Params: p `sessionID`: `Session.ID`; p `inboxID`: `Session.Message.ID`
  - Body: `{delivery: Session.Inbox.Delivery}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError; 409 ConflictError
  - Behavior: Conflict => 409 `{resource:<inboxID>, message:"Pending input cannot change to <delivery>: <inboxID>"}`.
  - Src: `packages/protocol/src/groups/session.ts:634`, `packages/server/src/handlers/session.ts:561`

**39. `GET /api/experimental/session/{sessionID}/instructions/entries`** (`experimental.session.instructions.entry.list`; internal `session.instructions.entry.list`) — List instruction entries.
  _List API-managed instruction entries attached to the session._
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: InstructionEntry.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:648`, `packages/server/src/handlers/session.ts:572`

**40. `PUT /api/experimental/session/{sessionID}/instructions/entries/{key}`** (`experimental.session.instructions.entry.put`; internal `session.instructions.entry.put`) — Put instruction entry.
  _Attach or replace one durable instruction entry. Changes announce as updates at the next step boundary._
  - Params: p `sessionID`: `Session.ID`; p `key`: `InstructionEntry.Key`
  - Body: `{value: unknown}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 413 InstructionEntryValueTooLargeError
  - Behavior: 413 InstructionEntryValueTooLargeError `{actualBytes,maxBytes,message}` raised by core.
  - Src: `packages/protocol/src/groups/session.ts:667`, `packages/server/src/handlers/session.ts:579`

**41. `DELETE /api/experimental/session/{sessionID}/instructions/entries/{key}`** (`experimental.session.instructions.entry.remove`; internal `session.instructions.entry.remove`) — Remove instruction entry.
  _Remove one instruction entry; the removal is announced to the model at the next step boundary._
  - Params: p `sessionID`: `Session.ID`; p `key`: `InstructionEntry.Key`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:688`, `packages/server/src/handlers/session.ts:587`

**42. `POST /api/session/{sessionID}/generate`** (`session.generate`; internal `session.generate`) — Generate text from session context.
  _Generate transient text from the current session context without mutating session history._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{prompt: string}`
  - Response: `200 SessionGenerateResponse`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Returns `{data:{text}}`. Non-NotFound errors => 503 `{service:"session generation"}`.
  - Src: `packages/protocol/src/groups/session.ts:708`, `packages/server/src/handlers/session.ts:595`

**43. `GET /api/experimental/session/{sessionID}/log`** (`session.log`; internal `session.log`) — Read the session log.
  _Experimental durable session event log. Reads events after an exclusive aggregate sequence and continues with live events when follow=true._
  - Params: p `sessionID`: `Session.ID`; q `after`?: `Event.SeqEncoded|null`; q `follow`?: `"true"|"false"|null`
  - Response: `200 text/event-stream {id?: string|null, event: string, data: SessionLogItemEncoded}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: SSE, see §8.2. Stream errors are converted to defects (`Stream.orDie`) => reserved failure event.
  - Src: `packages/protocol/src/groups/session.ts:726`, `packages/server/src/handlers/session.ts:610`

**44. `POST /api/session/{sessionID}/interrupt`** (`session.interrupt`; internal `session.interrupt`) — Interrupt session execution.
  _Interrupt active execution owned by this OpenCode process. Returns interrupted=true when an active execution was interrupted and false for the idle no-op. When resume=true, execution resumes pending steering input and next-in-line control items (manual compaction, moves) while queued prompts remain parked._
  - Params: p `sessionID`: `Session.ID`; q `resume`?: `"true"|"false"|null`
  - Response: `200 SessionInterruptResponse`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: Response `{interrupted: boolean}` (not wrapped in `data`).
  - Src: `packages/protocol/src/groups/session.ts:746`, `packages/server/src/handlers/session.ts:619`

**45. `POST /api/session/{sessionID}/background`** (`session.background`; internal `session.background`) — Background blocking session tools.
  _Move active foreground backgroundable tools for this session into background observation. Idle requests are a no-op._
  - Params: p `sessionID`: `Session.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:767`, `packages/server/src/handlers/session.ts:625`

**46. `GET /api/session/{sessionID}/message/{messageID}`** (`session.message.get`; internal `session.message`) — Get session message.
  _Retrieve one projected message owned by the Session._
  - Params: p `sessionID`: `Session.ID`; p `messageID`: `Session.Message.ID`
  - Response: `200 {data: Session.Message.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/MessageNotFoundError
  - Behavior: Missing => 404 MessageNotFoundError `Message not found: <id>`.
  - Src: `packages/protocol/src/groups/session.ts:783`, `packages/server/src/handlers/session.ts:632`

**47. `GET /api/session/{sessionID}/form`** (`session.form.list`; internal `session.form.list`) — List session forms.
  _Retrieve pending forms for a session._
  - Params: p `sessionID`: `string`
  - Response: `200 {data: Form.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: `sessionID` path param is a plain string: the literal `global` selects the request's location (undocumented MCP-elicitation hack).
  - Src: `packages/protocol/src/groups/session.ts:796`, `packages/server/src/handlers/session.ts:645`

**48. `POST /api/session/{sessionID}/form`** (`session.form.create`; internal `session.form.create`) — Create session form.
  _Create a form for a session._
  - Params: p `sessionID`: `string`
  - Body: `Form.CreatePayload`
  - Response: `200 {data: Form.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError; 409 ConflictError
  - Behavior: `Form.AlreadyExistsError` => 409; `Form.InvalidFormError` => 400 `{field:"fields"}`.
  - Src: `packages/protocol/src/groups/session.ts:811`, `packages/server/src/handlers/session.ts:652`

**49. `GET /api/session/{sessionID}/form/{formID}`** (`session.form.get`; internal `session.form.get`) — Get session form.
  _Retrieve a form and its current state for a session._
  - Params: p `sessionID`: `string`; p `formID`: `Form.ID`
  - Response: `200 {data: Form.Detail}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/FormNotFoundError/LocationNotFoundError
  - Behavior: Form whose `sessionID` differs => 404 FormNotFoundError. Response data = `{...Form.Info, state}`.
  - Src: `packages/protocol/src/groups/session.ts:827`, `packages/server/src/handlers/session.ts:674`

**50. `DELETE /api/session/{sessionID}/form/{formID}`** (`session.form.cancel`; internal `session.form.cancel`) — Cancel form.
  _Cancel a pending form, optionally telling the asker why it was not answered._
  - Params: p `sessionID`: `string`; p `formID`: `Form.ID`; q `message`?: `string|null`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/FormNotFoundError/LocationNotFoundError; 409 FormAlreadySettledError
  - Behavior: Already settled => 409 FormAlreadySettledError.
  - Src: `packages/protocol/src/groups/session.ts:858`, `packages/server/src/handlers/session.ts:700`

**51. `POST /api/session/{sessionID}/form/{formID}/reply`** (`session.form.reply`; internal `session.form.reply`) — Reply to form.
  _Submit an answer to a pending form._
  - Params: p `sessionID`: `string`; p `formID`: `Form.ID`
  - Body: `Form.Reply`
  - Response: `204`
  - Errors: 400 FormInvalidAnswerError/InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/FormNotFoundError/LocationNotFoundError; 409 FormAlreadySettledError
  - Behavior: 409 FormAlreadySettledError; 400 FormInvalidAnswerError.
  - Src: `packages/protocol/src/groups/session.ts:842`, `packages/server/src/handlers/session.ts:684`

**52. `PUT /api/session/{sessionID}/environment`** (`session.environment`; internal `session.environment`) — Set session environment.
  _Replace the process environment used by local shell commands for this session._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{variables: Record<string,string>}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Src: `packages/protocol/src/groups/session.ts:874`, `packages/server/src/handlers/session.ts:216`

**53. `POST /api/session/{sessionID}/view`** (`session.view`; internal `session.view`) — View session.
  _Mark the idle transition observed by the viewer as viewed._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{idle: number}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError
  - Behavior: `idle` is epoch ms (decoded to DateTime then back to ms).
  - Src: `packages/protocol/src/groups/session.ts:888`, `packages/server/src/handlers/session.ts:200`

**54. `GET /api/session/{sessionID}/message`** (`session.message.list`; internal `session.messages`) — Get session messages.
  _Retrieve projected messages for a session, optionally filtered by type. Items keep the requested order across pages; use cursor.next or cursor.previous to move through the ordered timeline, passing the same type filter on each page._
  - Params: p `sessionID`: `Session.ID`; q `limit`?: `string|null`; q `order`?: `"asc"|"desc"|null`; q `cursor`?: `string|null`; q `type`?: `"agent-switched"|"model-switched"|"location-switched"|"user"|"synthetic"|"system"|"skill"|"shell"|"assistant"|"compaction"|null`
  - Response: `200 SessionMessagesResponse`
  - Errors: 400 InvalidCursorError/InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError; 500 UnknownError
  - Behavior: `limit` default 50, `order` default `desc`. `cursor` + `order` together => 400 InvalidCursorError `Cursor cannot be combined with order`. Cursor = base64url(JSON `{id, order, direction}`) (§2.5).
  - Src: `packages/protocol/src/groups/message.ts:68`, `packages/server/src/handlers/message.ts:32`


### 7.6 `model`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 55 | GET | `/api/model` | `model.list` | L | 200 | `awaitPlugins + Model.available()` |
| 56 | GET | `/api/model/default` | `model.default` | L | 200 | `awaitPlugins + Model.default()` |

**55. `GET /api/model`** (`model.list`; internal `model.list`) — List models.
  _Retrieve the current snapshot of available models ordered by release date. The snapshot may precede initial plugin settlement._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Model.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: awaitPlugins = `Plugin.awaitActivation` with 5 s timeout => 503 `{service:"plugin", message:"Location plugins did not finish activating"}`.
  - Src: `packages/protocol/src/groups/model.ts:10`, `packages/server/src/handlers/model.ts:10`

**56. `GET /api/model/default`** (`model.default`; internal `model.default`) — Get default model.
  _Retrieve the model used when a session has no explicit model selection._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Model.Info|null}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: `data` may be `null`.
  - Src: `packages/protocol/src/groups/model.ts:26`, `packages/server/src/handlers/model.ts:18`


### 7.7 `generate`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 57 | POST | `/api/experimental/generate` | `experimental.generate.text` | G (fixed location = Global.config dir) | 200 | `awaitPlugins + Generate.text(payload)` |

**57. `POST /api/experimental/generate`** (`experimental.generate.text`; internal `generate.text`) — Generate text.
  _Run one stateless model generation using the server's base configuration and return the assistant text. Uses the base configuration's default model when none is specified._
  - Body: `{prompt: string, model?: Model.Ref|null}`
  - Response: `200 GenerateTextResponse`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Runs in the location whose directory is the global config dir. `Generate.ModelSelectionError` => 400; other => 503 `{service: error.service}`. Returns `{data:{text}}`.
  - Src: `packages/protocol/src/groups/generate.ts:8`, `packages/server/src/handlers/generate.ts:17`


### 7.8 `provider`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 58 | GET | `/api/provider` | `provider.list` | L | 200 | `Provider.available()` |
| 59 | GET | `/api/provider/{providerID}` | `provider.get` | L | 200 | `Provider.get(id)` |

**58. `GET /api/provider`** (`provider.list`; internal `provider.list`) — List providers.
  _Retrieve active AI providers so clients can show provider availability and configuration._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Provider.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/provider.ts:10`, `packages/server/src/handlers/provider.ts:11`

**59. `GET /api/provider/{providerID}`** (`provider.get`; internal `provider.get`) — Get provider.
  _Retrieve a single AI provider so clients can inspect its availability and endpoint settings._
  - Params: p `providerID`: `Provider.ID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Provider.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 ProviderNotFoundError/LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: 404 `Provider not found: <id>`.
  - Src: `packages/protocol/src/groups/provider.ts:25`, `packages/server/src/handlers/provider.ts:18`


### 7.9 `integration`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 60 | GET | `/api/integration` | `integration.list` | L | 200 | `Plugin.awaitActivation (no timeout) + Integration.list()` |
| 61 | GET | `/api/integration/{integrationID}` | `integration.get` | L | 200 | `Integration.get(id)` |
| 62 | POST | `/api/experimental/integration/wellknown` | `experimental.integration.wellknown.add` | L | 204 | `WellKnown.add(url) + Integration.reload()` |
| 63 | POST | `/api/integration/{integrationID}/connect/key` | `integration.connect.key` | L | 204 | `Integration.connection.key({integrationID,key,answer,label})` |
| 64 | POST | `/api/integration/{integrationID}/connect/external` | `integration.connect.external` | L | 204 | `Integration.connection.external(...)` |
| 65 | POST | `/api/integration/{integrationID}/connect/oauth` | `integration.oauth.connect` | L | 200 | `Integration.oauth.connect(...)` |
| 66 | GET | `/api/integration/{integrationID}/connect/oauth/{attemptID}` | `integration.oauth.status` | L | 200 | `Integration.oauth.status` |
| 67 | DELETE | `/api/integration/{integrationID}/connect/oauth/{attemptID}` | `integration.oauth.cancel` | L | 204 | `Integration.oauth.cancel` |
| 68 | POST | `/api/integration/{integrationID}/connect/oauth/{attemptID}/complete` | `integration.oauth.complete` | L | 204 | `Integration.oauth.complete({...,code})` |
| 69 | POST | `/api/integration/{integrationID}/connect/command` | `integration.command.connect` | L | 200 | `Integration.command.connect` |
| 70 | GET | `/api/integration/{integrationID}/connect/command/{attemptID}` | `integration.command.status` | L | 200 | `Integration.command.status` |
| 71 | DELETE | `/api/integration/{integrationID}/connect/command/{attemptID}` | `integration.command.cancel` | L | 204 | `Integration.command.cancel` |

**60. `GET /api/integration`** (`integration.list`; internal `integration.list`) — List integrations.
  _Retrieve available integrations and their authentication methods._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Integration.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/integration.ts:16`, `packages/server/src/handlers/integration.ts:30`

**61. `GET /api/integration/{integrationID}`** (`integration.get`; internal `integration.get`) — Get integration.
  _Retrieve one integration and its authentication methods._
  - Params: p `integrationID`: `Integration.ID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Integration.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/LocationNotFoundError
  - Behavior: 404 IntegrationNotFoundError.
  - Src: `packages/protocol/src/groups/integration.ts:30`, `packages/server/src/handlers/integration.ts:38`

**62. `POST /api/experimental/integration/wellknown`** (`experimental.integration.wellknown.add`; internal `integration.wellknown.add`) — Add wellknown integration.
  _Discover and persist an experimental wellknown integration source._
  - Params: q `location[directory]`?
  - Body: `{url: string}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Discovery failure => 400 `{kind:"well_known_discovery"}`.
  - Src: `packages/protocol/src/groups/integration.ts:46`, `packages/server/src/handlers/integration.ts:51`

**63. `POST /api/integration/{integrationID}/connect/key`** (`integration.connect.key`; internal `integration.connect.key`) — Connect with key.
  _Run a key authentication method and store the resulting credential._
  - Params: p `integrationID`: `Integration.ID`; q `location[directory]`?
  - Body: `{key: string, answer?: Form.Answer|null, label?: string|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/LocationNotFoundError
  - Behavior: Pre-check 404. Authorization failure => 400 `{kind:"integration_authorization", message: cause.message || "Authentication failed"}`.
  - Src: `packages/protocol/src/groups/integration.ts:62`, `packages/server/src/handlers/integration.ts:67`

**64. `POST /api/integration/{integrationID}/connect/external`** (`integration.connect.external`; internal `integration.connect.external`) — Connect with external credentials.
  _Run an external authentication method and store a reference to its credential source._
  - Params: p `integrationID`: `Integration.ID`; q `location[directory]`?
  - Body: `{methodID: Integration.MethodID, answer?: Form.Answer|null, label?: string|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/LocationNotFoundError
  - Behavior: Same mapping as connect.key.
  - Src: `packages/protocol/src/groups/integration.ts:83`, `packages/server/src/handlers/integration.ts:87`

**65. `POST /api/integration/{integrationID}/connect/oauth`** (`integration.oauth.connect`; internal `integration.oauth.connect`) — Begin OAuth connection.
  _Start an OAuth attempt and return the authorization details._
  - Params: p `integrationID`: `Integration.ID`; q `location[directory]`?
  - Body: `{methodID: Integration.MethodID, answer?: Form.Answer|null, label?: string|null}`
  - Response: `200 {location: Location.PublicRef, data: Integration.AttemptEncoded}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: No 404 pre-check (unknown integration surfaces as authorization error). Response wrapped in `{location,data}`.
  - Src: `packages/protocol/src/groups/integration.ts:104`, `packages/server/src/handlers/integration.ts:107`

**66. `GET /api/integration/{integrationID}/connect/oauth/{attemptID}`** (`integration.oauth.status`; internal `integration.oauth.status`) — Get OAuth attempt status.
  _Poll the current status of an OAuth attempt._
  - Params: p `integrationID`: `Integration.ID`; p `attemptID`: `Integration.AttemptID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Integration.AttemptStatus}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/IntegrationAttemptNotFoundError/LocationNotFoundError
  - Behavior: 404 IntegrationAttemptNotFoundError `OAuth attempt not found: <id>`.
  - Src: `packages/protocol/src/groups/integration.ts:125`, `packages/server/src/handlers/integration.ts:123`

**67. `DELETE /api/integration/{integrationID}/connect/oauth/{attemptID}`** (`integration.oauth.cancel`; internal `integration.oauth.cancel`) — Cancel OAuth connection.
  _Cancel an OAuth attempt and release its resources._
  - Params: p `integrationID`: `Integration.ID`; p `attemptID`: `Integration.AttemptID`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Idempotent; always 204.
  - Src: `packages/protocol/src/groups/integration.ts:162`, `packages/server/src/handlers/integration.ts:189`

**68. `POST /api/integration/{integrationID}/connect/oauth/{attemptID}/complete`** (`integration.oauth.complete`; internal `integration.oauth.complete`) — Complete OAuth connection.
  _Complete a code-based OAuth attempt and store the resulting credential._
  - Params: p `integrationID`: `Integration.ID`; p `attemptID`: `Integration.AttemptID`; q `location[directory]`?
  - Body: `{code?: string|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/IntegrationAttemptNotFoundError/LocationNotFoundError
  - Behavior: `Integration.CodeRequired` => 400 `{kind:"integration_code_required", message:"Authorization code is required"}`; other => 400 `{kind:"integration_authorization", message:"Authentication failed"}`.
  - Src: `packages/protocol/src/groups/integration.ts:141`, `packages/server/src/handlers/integration.ts:151`

**69. `POST /api/integration/{integrationID}/connect/command`** (`integration.command.connect`; internal `integration.command.connect`) — Begin command connection.
  _Start a command authentication attempt._
  - Params: p `integrationID`: `Integration.ID`; q `location[directory]`?
  - Body: `{methodID: Integration.MethodID, label?: string|null}`
  - Response: `200 {location: Location.PublicRef, data: Integration.CommandAttempt}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/IntegrationMethodNotFoundError/LocationNotFoundError
  - Behavior: Method must exist with `type:"command"` else 404 IntegrationMethodNotFoundError.
  - Src: `packages/protocol/src/groups/integration.ts:177`, `packages/server/src/handlers/integration.ts:200`

**70. `GET /api/integration/{integrationID}/connect/command/{attemptID}`** (`integration.command.status`; internal `integration.command.status`) — Get command attempt status.
  _Poll the current status and output of a command authentication attempt._
  - Params: p `integrationID`: `Integration.ID`; p `attemptID`: `Integration.AttemptID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Integration.CommandAttemptStatus}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 IntegrationNotFoundError/IntegrationAttemptNotFoundError/LocationNotFoundError
  - Behavior: 404 `Command attempt not found: <id>`.
  - Src: `packages/protocol/src/groups/integration.ts:197`, `packages/server/src/handlers/integration.ts:227`

**71. `DELETE /api/integration/{integrationID}/connect/command/{attemptID}`** (`integration.command.cancel`; internal `integration.command.cancel`) — Cancel command connection.
  _Cancel a command authentication attempt and terminate its process._
  - Params: p `integrationID`: `Integration.ID`; p `attemptID`: `Integration.AttemptID`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Idempotent.
  - Src: `packages/protocol/src/groups/integration.ts:213`, `packages/server/src/handlers/integration.ts:255`


### 7.10 `mcp`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 72 | GET | `/api/mcp` | `mcp.list` | L | 200 | `Mcp.servers()` |
| 73 | PUT | `/api/experimental/mcp/{server}` | `experimental.mcp.add` | L | 204 | `Mcp.add(server, config)` |
| 74 | DELETE | `/api/experimental/mcp/{server}` | `experimental.mcp.remove` | L | 204 | `Mcp.remove(server)` |
| 75 | POST | `/api/experimental/mcp/{server}/connect` | `experimental.mcp.connect` | L | 204 | `Mcp.connect(server)` |
| 76 | POST | `/api/experimental/mcp/{server}/disconnect` | `experimental.mcp.disconnect` | L | 204 | `Mcp.disconnect(server)` |
| 77 | GET | `/api/mcp/resource` | `mcp.resource.catalog` | L | 200 | `Mcp.resourceCatalog()` |

**72. `GET /api/mcp`** (`mcp.list`; internal `mcp.list`) — List MCP servers.
  _Retrieve configured MCP servers and their connection status._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Mcp.Server[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Projected to `{name, status, integrationID}`.
  - Src: `packages/protocol/src/groups/mcp.ts:10`, `packages/server/src/handlers/mcp.ts:14`

**73. `PUT /api/experimental/mcp/{server}`** (`experimental.mcp.add`; internal `mcp.add`) — Add MCP server.
  _Add an MCP server at runtime or replace an existing one, connecting it immediately._
  - Params: p `server`: `string`; q `location[directory]`?
  - Body: `{config: (Mcp.LocalConfigEncoded|Mcp.RemoteConfigEncoded)}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Runtime only; not persisted.
  - Src: `packages/protocol/src/groups/mcp.ts:24`, `packages/server/src/handlers/mcp.ts:29`

**74. `DELETE /api/experimental/mcp/{server}`** (`experimental.mcp.remove`; internal `mcp.remove`) — Remove MCP server.
  _Stop an MCP server and remove it from the runtime set until restart._
  - Params: p `server`: `string`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 McpServerNotFoundError/LocationNotFoundError
  - Behavior: 404 McpServerNotFoundError.
  - Src: `packages/protocol/src/groups/mcp.ts:42`, `packages/server/src/handlers/mcp.ts:37`

**75. `POST /api/experimental/mcp/{server}/connect`** (`experimental.mcp.connect`; internal `mcp.connect`) — Connect MCP server.
  _Connect an MCP server at runtime, overriding a disabled configuration until restart._
  - Params: p `server`: `string`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 McpServerNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/mcp.ts:58`, `packages/server/src/handlers/mcp.ts:45`

**76. `POST /api/experimental/mcp/{server}/disconnect`** (`experimental.mcp.disconnect`; internal `mcp.disconnect`) — Disconnect MCP server.
  _Disconnect an MCP server at runtime, removing its tools until reconnected._
  - Params: p `server`: `string`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 McpServerNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/mcp.ts:74`, `packages/server/src/handlers/mcp.ts:53`

**77. `GET /api/mcp/resource`** (`mcp.resource.catalog`; internal `mcp.resource.catalog`) — List MCP resources.
  _Retrieve resources and resource templates from connected MCP servers._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Mcp.ResourceCatalog}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/mcp.ts:90`, `packages/server/src/handlers/mcp.ts:61`


### 7.11 `credential`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 78 | GET | `/api/credential` | `credential.list` | G | 200 | `Credential.all()` |
| 79 | POST | `/api/credential` | `credential.create` | G | 200 | `Credential.get(id) + Credential.create + Credential.list(integrationID)` |
| 80 | PATCH | `/api/credential/{credentialID}` | `credential.update` | G | 204 | `Credential.update(id,{label})` |
| 81 | DELETE | `/api/credential/{credentialID}` | `credential.remove` | G | 204 | `Credential.remove(id)` |
| 82 | POST | `/api/credential/{credentialID}/activate` | `credential.activate` | G | 204 | `Credential.activate(id)` |

**78. `GET /api/credential`** (`credential.list`; internal `credential.list`) — List credentials.
  _List every stored integration credential, including its secret value._
  - Response: `200 {data: Credential.Entry[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Entry = `{id, integrationID, label, active, value}`; `active` is true for the LAST credential of each integration in the returned order (core orders the selected one last). Secret values are returned.
  - Src: `packages/protocol/src/groups/credential.ts:8`, `packages/server/src/handlers/credential.ts:9`

**79. `POST /api/credential`** (`credential.create`; internal `credential.create`) — Create credential.
  _Store an integration credential. It becomes the integration's active credential unless activate is false and the integration already has one. Fails with a conflict when the requested ID already exists._
  - Body: `Credential.CreateInput`
  - Response: `200 {data: Credential.Entry}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 409 ConflictError
  - Behavior: Existing `id` => 409 `Credential already exists: <id>`.
  - Src: `packages/protocol/src/groups/credential.ts:19`, `packages/server/src/handlers/credential.ts:16`

**80. `PATCH /api/credential/{credentialID}`** (`credential.update`; internal `credential.update`) — Update credential.
  _Update a stored credential label._
  - Params: p `credentialID`: `Credential.ID`
  - Body: `{label: string}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Src: `packages/protocol/src/groups/credential.ts:33`, `packages/server/src/handlers/credential.ts:30`

**81. `DELETE /api/credential/{credentialID}`** (`credential.remove`; internal `credential.remove`) — Remove credential.
  _Remove a stored integration credential._
  - Params: p `credentialID`: `Credential.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Src: `packages/protocol/src/groups/credential.ts:61`, `packages/server/src/handlers/credential.ts:46`

**82. `POST /api/credential/{credentialID}/activate`** (`credential.activate`; internal `credential.activate`) — Activate credential.
  _Activate a stored integration credential._
  - Params: p `credentialID`: `Credential.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Src: `packages/protocol/src/groups/credential.ts:48`, `packages/server/src/handlers/credential.ts:38`


### 7.12 `project`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 83 | GET | `/api/project` | `project.list` | L (unused) | 200 | `Project.list()` |
| 84 | PATCH | `/api/project/{projectID}` | `project.update` | L (unused) | 200 | `Project.update({...body, projectID})` |

**83. `GET /api/project`** (`project.list`; internal `project.list`) — List projects.
  _List known projects._
  - Response: `200 Project[]`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Bare array response (no envelope).
  - Src: `packages/protocol/src/groups/project.ts:11`, `packages/server/src/handlers/project.ts:9`

**84. `PATCH /api/project/{projectID}`** (`project.update`; internal `project.update`) — Update project.
  _Update the project canonical directory, display metadata, and workspace commands._
  - Params: p `projectID`: `Project.ID`
  - Body: `{canonical?: AbsolutePath, name?: string, icon?: Project.Icon, commands?: Project.Commands}`
  - Response: `200 Project`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 ProjectNotFoundError/LocationNotFoundError
  - Behavior: ANY failure => 404 ProjectNotFoundError. Bare `Project` response.
  - Src: `packages/protocol/src/groups/project.ts:22`, `packages/server/src/handlers/project.ts:10`


### 7.13 `form`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 85 | GET | `/api/form` | `form.list` | L | 200 | `Form.list()` |

**85. `GET /api/form`** (`form.list`; internal `form.list`) — List pending forms.
  _Retrieve pending forms for a location._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Form.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/form.ts:12`, `packages/server/src/handlers/form.ts:8`


### 7.14 `permission`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 86 | GET | `/api/permission/request` | `permission.request.list` | L | 200 | `Permission.list()` |
| 87 | GET | `/api/permission/saved` | `permission.saved.list` | L | 200 | `PermissionSaved.list({projectID: query.projectID ?? location.project.id})` |
| 88 | DELETE | `/api/permission/saved/{id}` | `permission.saved.remove` | L | 204 | `PermissionSaved.remove(id)` |
| 89 | POST | `/api/session/{sessionID}/permission` | `session.permission.create` | S | 200 | `Permission.ask({...})` |
| 90 | GET | `/api/session/{sessionID}/permission` | `session.permission.list` | H (session's location) | 200 | `Permission.forSession(sessionID)` |
| 91 | GET | `/api/session/{sessionID}/permission/{requestID}` | `session.permission.get` | S | 200 | `Permission.get(requestID)` |
| 92 | POST | `/api/session/{sessionID}/permission/{requestID}/reply` | `session.permission.reply` | S | 204 | `Permission.reply({requestID, reply: body.decision, message})` |

**86. `GET /api/permission/request`** (`permission.request.list`; internal `permission.request.list`) — List pending permission requests.
  _Retrieve pending permission requests for a location._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Permission.Request[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/permission.ts:23`, `packages/server/src/handlers/permission.ts:32`

**87. `GET /api/permission/saved`** (`permission.saved.list`; internal `permission.saved.list`) — List saved permissions.
  _Retrieve saved permissions, optionally filtered by project._
  - Params: q `projectID`?: `Project.ID|null`
  - Response: `200 {data: PermissionSaved.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/permission.ts:37`, `packages/server/src/handlers/permission.ts:86`

**88. `DELETE /api/permission/saved/{id}`** (`permission.saved.remove`; internal `permission.saved.remove`) — Remove saved permission.
  _Remove a saved permission by ID._
  - Params: p `id`: `PermissionSaved.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Idempotent.
  - Src: `packages/protocol/src/groups/permission.ts:49`, `packages/server/src/handlers/permission.ts:98`

**89. `POST /api/session/{sessionID}/permission`** (`session.permission.create`; internal `session.permission.create`) — Create permission request.
  _Evaluate and, when approval is required, create a permission request for a session._
  - Params: p `sessionID`: `Session.ID`
  - Body: `{id?: Permission.ID|null, action: string, resources: string[], save?: string[], metadata?: object, source?: Permission.Source, agent?: Agent.ID|null}`
  - Response: `200 {data: {id: Permission.ID, effect: Permission.Effect}}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: Non-blocking. Returns `{data:{id, effect}}`.
  - Src: `packages/protocol/src/groups/permission.ts:63`, `packages/server/src/handlers/permission.ts:39`

**90. `GET /api/session/{sessionID}/permission`** (`session.permission.list`; internal `session.permission.list`) — List session permission requests.
  _Retrieve pending permission requests owned by a session._
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: Permission.Request[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/LocationNotFoundError
  - Behavior: Handler resolves the session location itself (same errors as S).
  - Src: `packages/protocol/src/groups/permission.ts:89`, `packages/server/src/handlers/permission.ts:59`

**91. `GET /api/session/{sessionID}/permission/{requestID}`** (`session.permission.get`; internal `session.permission.get`) — Get permission request.
  _Retrieve a pending permission request owned by a session._
  - Params: p `sessionID`: `Session.ID`; p `requestID`: `Permission.ID`
  - Response: `200 {data: Permission.Request}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/PermissionNotFoundError/LocationNotFoundError
  - Behavior: Request absent or owned by another session => 404 PermissionNotFoundError `Permission request not found: <id>`.
  - Src: `packages/protocol/src/groups/permission.ts:102`, `packages/server/src/handlers/permission.ts:69`

**92. `POST /api/session/{sessionID}/permission/{requestID}/reply`** (`session.permission.reply`; internal `session.permission.reply`) — Reply to pending permission request.
  _Respond to a pending permission request owned by a session._
  - Params: p `sessionID`: `Session.ID`; p `requestID`: `Permission.ID`
  - Body: `{decision: Permission.Reply, message?: string|null}`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 SessionNotFoundError/PermissionNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/permission.ts:117`, `packages/server/src/handlers/permission.ts:76`


### 7.15 `filesystem`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 93 | GET | `/api/fs/read/*` | `fs.read` | L | 200 | `FileSystem.read({path})` |
| 94 | GET | `/api/fs/list` | `fs.list` | L | 200 | `FileSystem.list({path, location})` |
| 95 | GET | `/api/fs/find` | `fs.find` | L | 200 | `FileSystem.find({query,type,limit})` |
| 96 | POST | `/api/experimental/fs/write` | `experimental.fs.write` | L | 200 | `FileSystem.write({path, data: body})` |

**93. `GET /api/fs/read/*`** (`fs.read`; internal `fs.read`) — Read file.
  _Stream one file relative to the requested location with ETag, Last-Modified, and single-range HTTP Range support (206 Partial Content, 416 Range Not Satisfiable, and 304 Not Modified; multi-range and malformed Range headers fall back to 200)._
  - Params: q `location[directory]`?
  - Response: `200 application/octet-stream string(binary)`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 FileNotFoundError/LocationNotFoundError
  - Behavior: Raw handler; path = URL pathname after `/api/fs/read/`, percent-decoded (decode failure => 404). See §8.5 for caching/range.
  - Src: `packages/protocol/src/groups/fs.ts:33`, `packages/server/src/handlers/fs.ts:13`

**94. `GET /api/fs/list`** (`fs.list`; internal `fs.list`) — List directory.
  _List direct children using an absolute path or a path relative to the requested location, including parents and siblings outside its directory. Entry paths remain relative to the requested location; listing does not switch locations._
  - Params: q `location[directory]`?; q `path`?: `string|null`
  - Response: `200 {location: Location.PublicRef, data: FileSystem.Entry[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 FileNotFoundError/LocationNotFoundError
  - Behavior: NotFound => 404 FileNotFoundError `Directory not found: <p>`.
  - Src: `packages/protocol/src/groups/fs.ts:49`, `packages/server/src/handlers/fs.ts:27`

**95. `GET /api/fs/find`** (`fs.find`; internal `fs.find`) — Find files.
  _Find recursively ranked filesystem entries relative to the requested location._
  - Params: q `location[directory]`?; q `query`: `string`; q `type`?: `"file"|"directory"`; q `limit`?: `string|null`
  - Response: `200 {location: Location.PublicRef, data: FileSystem.Entry[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/fs.ts:65`, `packages/server/src/handlers/fs.ts:41`

**96. `POST /api/experimental/fs/write`** (`experimental.fs.write`; internal `fs.write`) — Write file.
  _Write the raw request body to an absolute path or a path relative to the requested location, creating parent directories, and return the resolved absolute path. Unlike read, the target is not confined to the location. Experimental: may change without compatibility guarantees._
  - Params: q `location[directory]`?; q `path`: `string`
  - Body: `application/octet-stream string(binary)`
  - Response: `200 {location: Location.PublicRef, data: FileSystem.Write}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Body is raw bytes (`application/octet-stream`).
  - Src: `packages/protocol/src/groups/fs.ts:79`, `packages/server/src/handlers/fs.ts:49`


### 7.16 `command`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 97 | GET | `/api/command` | `command.list` | L | 200 | `Command.list()` |

**97. `GET /api/command`** (`command.list`; internal `command.list`) — List commands.
  _Retrieve currently registered commands._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Command.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/command.ts:9`, `packages/server/src/handlers/command.ts:7`


### 7.17 `skill`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 98 | GET | `/api/skill` | `skill.list` | L | 200 | `Skill.list()` |

**98. `GET /api/skill`** (`skill.list`; internal `skill.list`) — List skills.
  _Retrieve currently registered skills._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Skill.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/skill.ts:9`, `packages/server/src/handlers/skill.ts:7`


### 7.18 `rpc`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 99 | POST | `/api/rpc/{rpcID}/{method}` | `rpc.call` | L | 200 | `Plugin.awaitActivation + Rpc.call(rpcID, method, input)` |

**99. `POST /api/rpc/{rpcID}/{method}`** (`rpc.call`; internal `rpc.call`) — Call a plugin RPC.
  _Dispatch a method to the currently registered RPC at the requested location._
  - Params: p `rpcID`: `string`; p `method`: `string`; q `location[directory]`?
  - Body: `Rpc.Input`
  - Response: `200 Rpc.Output`
  - Errors: 400 RpcError/InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 500 RpcInternalError
  - Behavior: See §8.4.
  - Src: `packages/protocol/src/groups/rpc.ts:14`, `packages/server/src/handlers/rpc.ts:9`


### 7.19 `event`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 100 | GET | `/api/event` | `event.subscribe` | G | 200 | `EventFeed (Bus.listen)` |

**100. `GET /api/event`** (`event.subscribe`; internal `event.subscribe`) — Subscribe to events.
  _Subscribe to native events and plugin RPC events across all server locations. Volatile by contract: a slow consumer overflows and fails the stream, and events during disconnection are missed._
  - Response: `200 text/event-stream {id?: string|null, event: string, data: V2EventEncoded}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: SSE, see §8.1.
  - Src: `packages/protocol/src/groups/event.ts:44`, `packages/server/src/handlers/event.ts:11`


### 7.20 `pty`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 101 | GET | `/api/pty` | `pty.list` | L | 200 | `Pty.list()` |
| 102 | POST | `/api/pty` | `pty.create` | L | 200 | `Pty.create({...body, cwd: body.cwd or location.directory, env: {...body.env, ...PtyEnvironment.get()}})` |
| 103 | GET | `/api/pty/{ptyID}` | `pty.get` | L | 200 | `Pty.get(id)` |
| 104 | PUT | `/api/pty/{ptyID}` | `pty.update` | L | 200 | `Pty.update(id, body)` |
| 105 | DELETE | `/api/pty/{ptyID}` | `pty.remove` | L | 204 | `Pty.remove(id)` |
| 106 | POST | `/api/pty/{ptyID}/connect-token` | `pty.connect.token` | L | 200 | `PtyTicket.issue({ptyID, directory, workspaceID})` |
| 107 | GET | `/api/pty/{ptyID}/connect` | `pty.connect` | H (raw; no location middleware) | 101 (WS) | `PtyTicket.consume + Pty.attach` |

**101. `GET /api/pty`** (`pty.list`; internal `pty.list`) — List PTY sessions.
  _List PTY sessions for a location, including exited sessions retained until removal._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Pty[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/pty.ts:26`, `packages/server/src/handlers/pty.ts:35`

**102. `POST /api/pty`** (`pty.create`; internal `pty.create`) — Create PTY session.
  _Create a pseudo-terminal session for a location._
  - Params: q `location[directory]`?
  - Body: `{command?: string, args?: string[], cwd?: string, title?: string, env?: Record<string,string>}`
  - Response: `200 {location: Location.PublicRef, data: Pty}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/pty.ts:40`, `packages/server/src/handlers/pty.ts:42`

**103. `GET /api/pty/{ptyID}`** (`pty.get`; internal `pty.get`) — Get PTY session.
  _Get one PTY session, including its exit code once exited._
  - Params: p `ptyID`: `Pty.ID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Pty}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError/LocationNotFoundError
  - Behavior: 404 `PTY session not found: <id>`.
  - Src: `packages/protocol/src/groups/pty.ts:55`, `packages/server/src/handlers/pty.ts:61`

**104. `PUT /api/pty/{ptyID}`** (`pty.update`; internal `pty.update`) — Update PTY session.
  _Update the title or viewport size of one PTY session._
  - Params: p `ptyID`: `Pty.ID`; q `location[directory]`?
  - Body: `{title?: string, size?: {rows: integer, cols: integer}}`
  - Response: `200 {location: Location.PublicRef, data: Pty}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/pty.ts:71`, `packages/server/src/handlers/pty.ts:79`

**105. `DELETE /api/pty/{ptyID}`** (`pty.remove`; internal `pty.remove`) — Remove PTY session.
  _Terminate and remove one PTY session._
  - Params: p `ptyID`: `Pty.ID`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/pty.ts:88`, `packages/server/src/handlers/pty.ts:102`

**106. `POST /api/pty/{ptyID}/connect-token`** (`pty.connect.token`; internal `pty.connectToken`) — Create PTY WebSocket token.
  _Create a short-lived single-use ticket for opening a PTY WebSocket connection._
  - Params: p `ptyID`: `Pty.ID`; h `x-opencode-ticket`?: `string|null`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: PtyTicket.ConnectToken}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 403 ForbiddenError; 404 PtyNotFoundError/LocationNotFoundError
  - Behavior: Requires header `x-opencode-ticket: 1` AND allowed Origin (or same host) else 403 `Invalid PTY connect token request`. Ticket = UUID v4, TTL 60 s, single use.
  - Src: `packages/protocol/src/groups/pty.ts:104`, `packages/server/src/handlers/pty.ts:119`

**107. `GET /api/pty/{ptyID}/connect`** (`pty.connect`; internal `pty.connect`) — Connect to PTY session.
  _Establish a WebSocket connection streaming PTY output and accepting terminal input._
  - Params: p `ptyID`: `Pty.ID`; q `location[directory]`?: `string`; q `cursor`?: `string`; q `ticket`?: `string`
  - Response: `200 boolean` — actually a WebSocket upgrade (`x-websocket: true`)
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 403 ForbiddenError; 404 PtyNotFoundError/LocationNotFoundError
  - Behavior: WebSocket; see §9.1.
  - Src: `packages/protocol/src/groups/pty.ts:123`, `packages/server/src/handlers/pty.ts:144`


### 7.21 `persistentPty`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 108 | GET | `/api/experimental/session/{sessionID}/terminal/read` | `server.experimental.persistentPty.read` | G | 200 | `PersistentPty.read(sessionID, lines)` |
| 109 | GET | `/api/experimental/session/{sessionID}/terminal` | `server.experimental.persistentPty.list` | G | 200 | `PersistentPty.list(sessionID)` |
| 110 | POST | `/api/experimental/session/{sessionID}/terminal` | `server.experimental.persistentPty.create` | G | 200 | `PersistentPty.create(sessionID, {command,args,cwd,title,env,cols,rows})` |
| 111 | POST | `/api/experimental/persistent-pty/shutdown` | `server.experimental.persistentPty.shutdown` | G | 204 | `PersistentPty.shutdown()` |
| 112 | POST | `/api/experimental/persistent-pty/handoff` | `server.experimental.persistentPty.handoff` | G | 200 | `PersistentPty.handoff()` |
| 113 | GET | `/api/experimental/persistent-pty/{ptyID}` | `server.experimental.persistentPty.get` | G | 200 | `PersistentPty.get(id)` |
| 114 | PUT | `/api/experimental/persistent-pty/{ptyID}` | `server.experimental.persistentPty.update` | G | 200 | `PersistentPty.resize(id, cols, rows, attachmentID) + get` |
| 115 | DELETE | `/api/experimental/persistent-pty/{ptyID}` | `server.experimental.persistentPty.remove` | G | 204 | `PersistentPty.remove(id)` |
| 116 | GET | `/api/experimental/persistent-pty/{ptyID}/snapshot` | `server.experimental.persistentPty.snapshot` | G | 200 | `PersistentPty.snapshot(id)` |
| 117 | POST | `/api/experimental/persistent-pty/{ptyID}/connect-token` | `server.experimental.persistentPty.connectToken` | G | 200 | `PtyTicket.issue({ptyID})` |
| 118 | GET | `/api/experimental/persistent-pty/{ptyID}/connect` | `persistentPty.connect` | G (raw) | 101 (WS) | `PtyTicket.consume + PersistentPty.attach` |

**108. `GET /api/experimental/session/{sessionID}/terminal/read`** (`server.experimental.persistentPty.read`; internal `persistentPty.read`) — Read the session's most recently controlled terminal.
  _Read the last physical rows without changing selection or taking control. Omitted lines uses the live terminal height; larger counts include retained history. Blank rows are preserved. Screen dimensions and cursor remain relative to the live screen. Returns null when no current terminal exists. Selection is server-local and resets on restart. Experimental: may change without compatibility guarantees._
  - Params: p `sessionID`: `Session.ID`; q `lines`?: `PersistentPty.ReadLinesEncoded|null`
  - Response: `200 {data: PersistentPty.ReadResult|null}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Behavior: `data: null` when no current terminal.
  - Src: `packages/protocol/src/groups/persistent-pty.ts:23`, `packages/server/src/handlers/persistent-pty.ts:24`

**109. `GET /api/experimental/session/{sessionID}/terminal`** (`server.experimental.persistentPty.list`; internal `persistentPty.list`) — (no summary in OpenAPI).
  - Params: p `sessionID`: `Session.ID`
  - Response: `200 {data: PersistentPty.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:39`, `packages/server/src/handlers/persistent-pty.ts:30`

**110. `POST /api/experimental/session/{sessionID}/terminal`** (`server.experimental.persistentPty.create`; internal `persistentPty.create`) — (no summary in OpenAPI).
  - Params: p `sessionID`: `Session.ID`
  - Body: `PersistentPty.CreateInput`
  - Response: `200 {data: PersistentPty.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:46`, `packages/server/src/handlers/persistent-pty.ts:36`

**111. `POST /api/experimental/persistent-pty/shutdown`** (`server.experimental.persistentPty.shutdown`; internal `persistentPty.shutdown`) — (no summary in OpenAPI).
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:54`, `packages/server/src/handlers/persistent-pty.ts:54`

**112. `POST /api/experimental/persistent-pty/handoff`** (`server.experimental.persistentPty.handoff`; internal `persistentPty.handoff`) — (no summary in OpenAPI).
  - Response: `200 {handoff: PersistentPty.Handoff|null}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:60`, `packages/server/src/handlers/persistent-pty.ts:61`

**113. `GET /api/experimental/persistent-pty/{ptyID}`** (`server.experimental.persistentPty.get`; internal `persistentPty.get`) — (no summary in OpenAPI).
  - Params: p `ptyID`: `Pty.ID`
  - Response: `200 {data: PersistentPty.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:66`, `packages/server/src/handlers/persistent-pty.ts:67`

**114. `PUT /api/experimental/persistent-pty/{ptyID}`** (`server.experimental.persistentPty.update`; internal `persistentPty.update`) — (no summary in OpenAPI).
  - Params: p `ptyID`: `Pty.ID`
  - Body: `PersistentPty.UpdateInput`
  - Response: `200 {data: PersistentPty.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:73`, `packages/server/src/handlers/persistent-pty.ts:73`

**115. `DELETE /api/experimental/persistent-pty/{ptyID}`** (`server.experimental.persistentPty.remove`; internal `persistentPty.remove`) — (no summary in OpenAPI).
  - Params: p `ptyID`: `Pty.ID`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:88`, `packages/server/src/handlers/persistent-pty.ts:88`

**116. `GET /api/experimental/persistent-pty/{ptyID}/snapshot`** (`server.experimental.persistentPty.snapshot`; internal `persistentPty.snapshot`) — (no summary in OpenAPI).
  - Params: p `ptyID`: `Pty.ID`
  - Response: `200 {data: PersistentPty.Snapshot}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/persistent-pty.ts:81`, `packages/server/src/handlers/persistent-pty.ts:82`

**117. `POST /api/experimental/persistent-pty/{ptyID}/connect-token`** (`server.experimental.persistentPty.connectToken`; internal `persistentPty.connectToken`) — (no summary in OpenAPI).
  - Params: p `ptyID`: `Pty.ID`; h `x-opencode-ticket`?: `string|null`
  - Response: `200 {data: PtyTicket.ConnectToken}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 403 ForbiddenError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Behavior: Same header/origin gate as pty.connect.token (message `Invalid persistent PTY connect token request`).
  - Src: `packages/protocol/src/groups/persistent-pty.ts:95`, `packages/server/src/handlers/persistent-pty.ts:95`

**118. `GET /api/experimental/persistent-pty/{ptyID}/connect`** (`persistentPty.connect`; internal `persistentPty.connect`) — Connect to a persistent PTY.
  _Stream persistent PTY output through the OpenCode server._
  - Params: p `ptyID`: `Pty.ID`; q `cursor`?: `string`; q `role`?: `string`; q `attachment_id`?: `string`; q `takeover`?: `string`; q `input_protocol`?: `string`; q `ticket`?: `string`
  - Response: `200 boolean` — actually a WebSocket upgrade (`x-websocket: true`)
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 403 ForbiddenError; 404 PtyNotFoundError; 503 ServiceUnavailableError
  - Behavior: WebSocket; see §9.2.
  - Src: `packages/protocol/src/groups/persistent-pty.ts:103`, `packages/server/src/handlers/persistent-pty.ts:108`


### 7.22 `shell`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 119 | GET | `/api/shell` | `shell.list` | L | 200 | `Shell.list()` |
| 120 | POST | `/api/shell` | `shell.create` | L | 200 | `Shell.create({...body, cwd: body.cwd or location.directory})` |
| 121 | GET | `/api/shell/{id}` | `shell.get` | L | 200 | `Shell.get(id)` |
| 122 | DELETE | `/api/shell/{id}` | `shell.remove` | L | 204 | `Shell.remove(id)` |
| 123 | GET | `/api/shell/{id}/output` | `shell.output` | L | 200 | `Shell.output(id, {cursor, limit})` |

**119. `GET /api/shell`** (`shell.list`; internal `shell.list`) — List running shell commands.
  _List currently running shell commands for a location. Exited commands are not included._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Shell.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/shell.ts:10`, `packages/server/src/handlers/shell.ts:12`

**120. `POST /api/shell`** (`shell.create`; internal `shell.create`) — Run shell command.
  _Spawn one non-interactive shell command for a location. Combined stdout/stderr is captured to a file pageable via output._
  - Params: q `location[directory]`?
  - Body: `{command: string, cwd?: string, timeout?: integer, metadata?: object}`
  - Response: `200 {location: Location.PublicRef, data: Shell.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Core errors become defects (500).
  - Src: `packages/protocol/src/groups/shell.ts:24`, `packages/server/src/handlers/shell.ts:19`

**121. `GET /api/shell/{id}`** (`shell.get`; internal `shell.get`) — Get shell command.
  _Get one shell command, including its status and exit code once exited._
  - Params: p `id`: `Shell.ID`; q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Shell.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 ShellNotFoundError/LocationNotFoundError
  - Behavior: 404 `Shell command not found: <id>`.
  - Src: `packages/protocol/src/groups/shell.ts:40`, `packages/server/src/handlers/shell.ts:29`

**122. `DELETE /api/shell/{id}`** (`shell.remove`; internal `shell.remove`) — Remove shell command.
  _Terminate and remove one shell command and its retained output._
  - Params: p `id`: `Shell.ID`; q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Missing is a no-op (204).
  - Src: `packages/protocol/src/groups/shell.ts:72`, `packages/server/src/handlers/shell.ts:63`

**123. `GET /api/shell/{id}/output`** (`shell.output`; internal `shell.output`) — Read shell output.
  _Page through captured combined output by absolute byte cursor._
  - Params: p `id`: `Shell.ID`; q `location[directory]`?; q `cursor`?: `string`; q `limit`?: `string`
  - Response: `200 {location: Location.PublicRef, data: {output: string, cursor: integer, size: integer, truncated: boolean}}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 ShellNotFoundError/LocationNotFoundError
  - Src: `packages/protocol/src/groups/shell.ts:56`, `packages/server/src/handlers/shell.ts:46`


### 7.23 `reference`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 124 | GET | `/api/reference` | `reference.list` | L | 200 | `Reference.list()` |

**124. `GET /api/reference`** (`reference.list`; internal `reference.list`) — List references.
  _List references available in the requested location._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Reference.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/reference.ts:24`, `packages/server/src/handlers/reference.ts:7`


### 7.24 `worktree`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 125 | GET | `/api/worktree` | `worktree.list` | G | 200 | `Worktree.list({projectID})` |
| 126 | POST | `/api/worktree` | `worktree.create` | G | 200 | `Worktree.create(body)` |
| 127 | DELETE | `/api/worktree` | `worktree.remove` | G | 204 | `Worktree.remove(body)` |
| 128 | POST | `/api/worktree/refresh` | `worktree.refresh` | G | 204 | `Worktree.refresh(body)` |

**125. `GET /api/worktree`** (`worktree.list`; internal `worktree.list`) — List worktrees.
  _Return the project's saved worktree inventory without loading configuration or running discovery._
  - Params: q `projectID`: `Project.ID`
  - Response: `200 Worktree.List`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 ProjectNotFoundError
  - Behavior: Bare response.
  - Src: `packages/protocol/src/groups/worktree.ts:23`, `packages/server/src/handlers/worktree.ts:14`

**126. `POST /api/worktree`** (`worktree.create`; internal `worktree.create`) — Create worktree.
  _Load the project's canonical configuration, create a local worktree using its selected strategy, then run the project's setup script._
  - Body: `Worktree.CreateInput`
  - Response: `200 Worktree.Info`
  - Errors: 400 WorktreeError/InvalidRequestError; 401 UnauthorizedError; 404 ProjectNotFoundError
  - Behavior: Errors => 400 `WorktreeError{name:"WorktreeError", data:{message, forceRequired?}}` (not `_tag`-only shape).
  - Src: `packages/protocol/src/groups/worktree.ts:37`, `packages/server/src/handlers/worktree.ts:17`

**127. `DELETE /api/worktree`** (`worktree.remove`; internal `worktree.remove`) — Remove worktree.
  _Load the project's canonical configuration and remove a saved worktree using its recorded strategy._
  - Body: `Worktree.RemoveInput`
  - Response: `204`
  - Errors: 400 WorktreeError/InvalidRequestError; 401 UnauthorizedError; 404 ProjectNotFoundError
  - Behavior: DELETE with JSON body.
  - Src: `packages/protocol/src/groups/worktree.ts:51`, `packages/server/src/handlers/worktree.ts:18`

**128. `POST /api/worktree/refresh`** (`worktree.refresh`; internal `worktree.refresh`) — Refresh worktrees.
  _Load the project's canonical configuration, discover worktrees across known checkout roots using all available strategies, and reconcile saved state._
  - Body: `{projectID: Project.ID}`
  - Response: `204`
  - Errors: 400 WorktreeError/InvalidRequestError; 401 UnauthorizedError; 404 ProjectNotFoundError
  - Src: `packages/protocol/src/groups/worktree.ts:65`, `packages/server/src/handlers/worktree.ts:21`


### 7.25 `vcs`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 129 | POST | `/api/vcs/init` | `vcs.init` | L | 204 | `Plugin.awaitActivation + Vcs.initialize(provider ?? "git") + Project.resolve + LocationServiceMap.invalidate + bus.publish(worktree.updated)` |
| 130 | GET | `/api/vcs` | `vcs.get` | L | 200 | `Vcs.info()` |
| 131 | GET | `/api/vcs/base` | `vcs.base` | L | 200 | `Vcs.base()` |
| 132 | GET | `/api/vcs/status` | `vcs.status` | L | 200 | `Vcs.status()` |
| 133 | GET | `/api/vcs/branch` | `vcs.branch.list` | L | 200 | `Vcs.branches({search, limit: min(limit ?? 50, 100)})` |
| 134 | GET | `/api/vcs/diff` | `vcs.diff` | L | 200 | `Vcs.diff(mode, {context, base})` |

**129. `POST /api/vcs/init`** (`vcs.init`; internal `vcs.init`) — Initialize VCS repository.
  _Initialize a repository using the selected VCS provider in a markerless project's directory and refresh its location services. Omitting provider defaults to git; built-in git and hg providers support initialization. An unknown provider returns 400; a registered provider without init returns 501._
  - Params: q `location[directory]`?; q `provider`?: `string|null`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 409 ConflictError; 501 VcsInitNotSupportedError; 503 ServiceUnavailableError
  - Behavior: Error kinds: missing => 400 `{field:"location"}`; conflict => 409; unknown => 400 `{field:"provider"}`; unsupported => 501 VcsInitNotSupportedError; other => 503.
  - Src: `packages/protocol/src/groups/vcs.ts:36`, `packages/server/src/handlers/vcs.ts:21`

**130. `GET /api/vcs`** (`vcs.get`; internal `vcs.get`) — VCS info.
  _Get current and default branch information for the requested location._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Vcs.Info}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/vcs.ts:52`, `packages/server/src/handlers/vcs.ts:57`

**131. `GET /api/vcs/base`** (`vcs.base`; internal `vcs.base`) — VCS review base.
  _Infer a local review base from named branch creation history, or the repository default only when currently on that branch. Returns null before the first commit or when the provider lacks base metadata; ambiguous Git history requires an explicit base on diff requests._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Vcs.Base|null}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Error => 503 `{service:"vcs"}`.
  - Src: `packages/protocol/src/groups/vcs.ts:66`, `packages/server/src/handlers/vcs.ts:65`

**132. `GET /api/vcs/status`** (`vcs.status`; internal `vcs.status`) — VCS status.
  _List uncommitted working-copy changes relative to the requested location._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: Vcs.FileStatus[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/vcs.ts:82`, `packages/server/src/handlers/vcs.ts:75`

**133. `GET /api/vcs/branch`** (`vcs.branch.list`; internal `vcs.branch.list`) — VCS branches.
  _List local and remote branches available at the requested location._
  - Params: q `location[directory]`?; q `search`?: `string|null`; q `limit`?: `string|null`
  - Response: `200 {location: Location.PublicRef, data: Vcs.BranchList}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Src: `packages/protocol/src/groups/vcs.ts:96`, `packages/server/src/handlers/vcs.ts:83`

**134. `GET /api/vcs/diff`** (`vcs.diff`; internal `vcs.diff`) — VCS diff.
  _Diff HEAD to the working copy (working), the base merge-base to the working copy (branch), or the base merge-base to HEAD (committed). Omitting base preserves repository-default comparison; supplying it overrides the comparison without saving it._
  - Params: q `location[directory]`?; q `mode`: `Vcs.Mode`; q `base`?: `string`; q `context`?: `string|null`
  - Response: `200 {location: Location.PublicRef, data: FileDiff.Info[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: Error => 503 `{service:"vcs"}`.
  - Src: `packages/protocol/src/groups/vcs.ts:110`, `packages/server/src/handlers/vcs.ts:91`


### 7.26 `debug`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 135 | GET | `/api/debug/location` | `debug.location.list` | G | 200 | `RcMap.keys(LocationServiceMap.rcMap)` |
| 136 | DELETE | `/api/debug/location` | `debug.location.evict` | G | 204 | `LocationServiceMap.invalidate(requestRef(request))` |

**135. `GET /api/debug/location`** (`debug.location.list`; internal `debug.location`) — List loaded locations.
  _List locations currently loaded by the server._
  - Response: `200 Location.PublicRef[]`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Bare array of `{directory}`.
  - Src: `packages/protocol/src/groups/debug.ts:8`, `packages/server/src/handlers/debug.ts:9`

**136. `DELETE /api/debug/location`** (`debug.location.evict`; internal `debug.location.evict`) — Evict a loaded location.
  _Dispose the requested location's cached services so its next use boots them fresh._
  - Params: q `location[directory]`?
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Uses the same location resolution as L but does not boot it.
  - Src: `packages/protocol/src/groups/debug.ts:19`, `packages/server/src/handlers/debug.ts:16`


### 7.27 `migration`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 137 | GET | `/api/experimental/migration/v1` | `experimental.migration.v1.status` | G | 200 | `V1Migration.status()` |

**137. `GET /api/experimental/migration/v1`** (`experimental.migration.v1.status`; internal `migration.v1.status`) — Get V1 migration status.
  _Return the progress of the V1 to V2 session history migration._
  - Response: `200 {status: "required"|"completed"} | {status: "running", progress: {label: string, numerator?: integer|null, denominator?: integer|null}} | {status: "error", error: string}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError
  - Behavior: Bare response.
  - Src: `packages/protocol/src/groups/migration.ts:18`, `packages/server/src/handlers/migration.ts:7`


### 7.28 `websearch`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 138 | GET | `/api/websearch/provider` | `websearch.providers` | L | 200 | `WebSearch.providers()` |
| 139 | POST | `/api/websearch` | `websearch.query` | L | 200 | `WebSearch.query(body)` |

**138. `GET /api/websearch/provider`** (`websearch.providers`; internal `websearch.providers`) — List web search providers.
  _Return the registered web search providers._
  - Params: q `location[directory]`?
  - Response: `200 {location: Location.PublicRef, data: WebSearch.Provider[]}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Src: `packages/protocol/src/groups/websearch.ts:10`, `packages/server/src/handlers/websearch.ts:11`

**139. `POST /api/websearch`** (`websearch.query`; internal `websearch.query`) — Search the web.
  _Run one web search through the selected provider. Specify a provider to override the configured default._
  - Params: q `location[directory]`?
  - Body: `{query: string, providerID?: WebSearch.ID}`
  - Response: `200 {location: Location.PublicRef, data: WebSearch.ResponseEncoded}`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError; 503 ServiceUnavailableError
  - Behavior: ProviderRequired => 400 `{kind:"websearch_provider_required", field:"providerID"}`; ProviderNotFound => 400 `{kind:"websearch_provider_not_found"}`; Disabled => 400 `{kind:"websearch_disabled"}`; request failure => 503 `{service: providerID}`.
  - Src: `packages/protocol/src/groups/websearch.ts:25`, `packages/server/src/handlers/websearch.ts:18`


### 7.29 `config`

| # | Method | Path | operationId | Scope | OK | Core call |
|---|---|---|---|---|---|---|
| 140 | GET | `/api/config` | `config.get` | L | 200 | `Config.entries()` |
| 141 | GET | `/api/config/shell` | `config.shells` | L | 200 | `ShellSelect.list()` |
| 142 | PATCH | `/api/experimental/config` | `experimental.config.update` | L | 204 | `Config.update(body)` |

**140. `GET /api/config`** (`config.get`; internal `config.get`) — Get configuration.
  _Return configuration documents and discovery sources for the requested location, from lowest to highest priority._
  - Params: q `location[directory]`?
  - Response: `200 Config.Entry[]`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Bare array.
  - Src: `packages/protocol/src/groups/config.ts:9`, `packages/server/src/handlers/config.ts:9`

**141. `GET /api/config/shell`** (`config.shells`; internal `config.shells`) — List available shells.
  _Return shells available to terminal and agent execution._
  - Response: `200 ConfigShell.Option[]`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Bare array; unavailable => defect (500).
  - Src: `packages/protocol/src/groups/config.ts:24`, `packages/server/src/handlers/config.ts:18`

**142. `PATCH /api/experimental/config`** (`experimental.config.update`; internal `config.update`) — Update global configuration.
  _Patch supported fields in the highest-precedence global configuration document._
  - Body: `Config.Patch`
  - Response: `204`
  - Errors: 400 InvalidRequestError; 401 UnauthorizedError; 404 LocationNotFoundError
  - Behavior: Unavailable or failure => defect (500).
  - Src: `packages/protocol/src/groups/config.ts:35`, `packages/server/src/handlers/config.ts:10`


---

## 8. Streaming and raw endpoints

### 8.1 Global event feed — `GET /api/event` (SSE)

Handler: `packages/server/src/handlers/event.ts:11-34`; feed: `packages/server/src/event-feed.ts`.
Auth: normal (no ticket/query exemption; browsers use cookie or `auth_token`). Scope **G**: one
feed for all locations.

Response: `200`, headers `content-type: text/event-stream`,
`Cache-Control: no-cache, no-transform`, `X-Accel-Buffering: no`, `X-Content-Type-Options: nosniff`.
Not compressed even if `Accept-Encoding: br` (process.test.ts:96-104).

Framing (event-feed.ts:29-31): every event is exactly one line `data: <JSON>` followed by a blank
line, i.e. `"data: " + JSON.stringify(event) + "\n\n"`. **No `event:`, `id:` or `retry:` fields.**
Heartbeat: an SSE comment `": heartbeat\n\n"` every 15 s (`Stream.tick("15 seconds")`,
event.ts:21), merged with `haltStrategy: "left"` (the response ends when the event stream ends).

Sequence:

1. Subscription is registered (bounded dropping queue, capacity 4096, event-feed.ts:8,74-80).
2. First frame is a synthetic connected event (not from the bus, no `created`):
   `data: {"id":"evt_<ascending>","type":"server.connected","data":{}}`.
3. Then every bus event for which `isOpenCodeEvent` holds
   (`packages/protocol/src/groups/event.ts:67-68`): `type == "server.connected"`, or `type` is in
   `EventManifest.ServerDefinitions` (`packages/schema/src/event-manifest.ts:71-84`), or `type`
   starts with `rpc.`. Events published while nobody is subscribed are dropped.
4. Payload is the in-memory bus payload serialized with plain `JSON.stringify`
   (NOT schema-encoded): `{id, created:<epoch ms>, metadata?, type, durable?:{aggregateID, seq,
   version}, location?:{directory, workspaceID?}, data}` (`packages/schema/src/event.ts:67-78,
   108-149`). Plugin RPC events: `{id, created, metadata?, type:"rpc.<…>", location:{directory},
   data:{…}}` (groups/event.ts:14-21).

Server-visible event types (ServerDefinitions; full schemas belong to the events spec):
`location.shutdown`, `models-dev.refreshed`, `credential.updated`, `credential.switched`,
`integration.updated`, `provider.updated`, `model.updated`, `agent.updated`, all `session.*`
durable/ephemeral events from `packages/schema/src/session-event.ts` (session.created …
session.revert.committed, incl. ephemeral `session.text.delta`, `session.reasoning.delta`,
`session.tool.input.delta`, `session.tool.progress`, `session.compaction.delta`,
`session.usage.updated`), `filesystem.changed`, `reference.updated`, `permission.asked`,
`permission.replied`, `plugin.updated`, `project.updated`, `worktree.updated`,
`worktree.resolved`, `command.updated`, `config.updated`, `skill.updated`, `pty.created`,
`pty.updated`, `pty.exited`, `pty.deleted`, `persistent-pty.added`, `persistent-pty.removed`,
`shell.created`, `shell.exited`, `shell.deleted`, `form.created`, `form.replied`,
`form.cancelled`, `websearch.updated`, `session.status`, `session.idle`, `tui.prompt.append`,
`tui.command.execute`, `tui.toast.show`, `tui.session.select`, `installation.updated`,
`installation.update-available`, `vcs.branch.updated`, `mcp.status.changed`,
`mcp.resources.changed`, plus `server.connected` and `rpc.*`.
NOT forwarded: `lsp.updated`, `mcp.tools.changed`, `workspace.*`, `worktree.ready/failed`,
`session.compacted`, `global.disposed`, legacy v1 events.

Reliability contract ("volatile", groups/event.ts:51): no replay, no `Last-Event-ID`, no cursor.
If a subscriber's queue is full (4096 pending frames) it is removed and its stream fails
(`EventFeed.SubscriberOverflow`), which terminates the HTTP response abruptly (no failure event,
because this endpoint is `handleRaw`). If `JSON.stringify` throws for an event, **all**
subscribers are failed (event-feed.ts:41-62). Clients reconnect and re-fetch state; the
`server.connected` frame is their resync signal.

### 8.2 Session log — `GET /api/experimental/session/{sessionID}/log` (SSE)

Declared with `HttpApiSchema.StreamSse({data: Union(SessionEvent.Durable, EventLog.Synced)})`
(`packages/protocol/src/groups/session.ts:726-743`); handler `handlers/session.ts:610-618`.
Query: `after` (decimal string, exclusive aggregate `seq`), `follow` (`"true"|"false"`).
Session existence is checked first (404 JSON before streaming).

Framing (Effect SSE encoder, "data" mode): each item is `data: <schema-encoded JSON>\n\n`
(event name `message` is omitted, no `id:` line; multi-line data would be split into multiple
`data:` lines). Items are durable session events (with `durable.seq`) or sync markers
`{"type":"log.synced","aggregateID":"ses_…","seq":N}` (see `packages/server/test/sse-failure.test.ts:17`).
A mid-stream failure (stream errors are turned into defects by `Stream.orDie`) is emitted as a
final event:

```
event: effect/http-api/stream/failure
data: [{"_tag":"Die","defect":<encoded defect>}]

```

(OpenAPI `x-effect-stream.failureEvent`; asserted by sse-failure.test.ts:44-51). The cause array
items are `{_tag:"Fail",error}` | `{_tag:"Die",defect}` | `{_tag:"Interrupt",fiberId:number|null}`.
Resumption: client re-requests with `after=<last seq seen>`.

### 8.3 `GET /openapi.json`

Served by `HttpApiBuilder.layer(Api, {openapiPath:"/openapi.json"})` (routes.ts:176), generated
at runtime from the server `Api` (not the client one). Requires auth in process mode. The Rust
server can serve the checked-in `packages/protocol/openapi.json` (see Q12).

### 8.4 Plugin RPC — `POST /api/rpc/{rpcID}/{method}`

`packages/protocol/src/groups/rpc.ts:12-30`, handler `packages/server/src/handlers/rpc.ts:9-31`.
Scope **L**. Body `{"input"?: <any JSON>}`. Waits for `Plugin.awaitActivation` (no timeout), then
`Rpc.call(rpcID, method, input)`.

* Success `200`: `{"output": <any>}`, or `{}` when the RPC returned `undefined`.
* Typed RPC failure with `type` not in {`rpc.internal`,`rpc.invalid_output`} → `400
  {"_tag":"RpcError","type":…,"message":…,"data"?:…}` (`data` key omitted when undefined).
* `type` `rpc.internal` / `rpc.invalid_output` → `500 {"_tag":"RpcInternalError","type","message"}`.
* Any defect → logged, `500 {"_tag":"RpcInternalError","type":"rpc.internal","message":"RPC call failed"}`.

RPC events (`rpc.*`) are delivered on `/api/event` (§8.1).

### 8.5 Raw file read — `GET /api/fs/read/<relative path>`

Handler `packages/server/src/handlers/fs.ts:13-26,60-141`. Scope **L**. Path = URL pathname
minus the 13-char prefix `/api/fs/read/`, `decodeURIComponent`-ed (failure → 404
`FileNotFoundError` with the raw path). Confinement/resolution is done by `FileSystem.read`
(core spec). Not found → `404 {"_tag":"FileNotFoundError","path","message":"File not found: <p>"}`.

Response headers always: `cache-control: no-cache, no-transform`,
`etag: W/"<size hex>-<mtime ms hex | 0>"`, `last-modified: <mtime UTC string>` (if mtime known).

1. `If-None-Match` present: 304 if any comma-separated entry is `*` or equals the etag ignoring
   a `W/` prefix (case-insensitive). Else, if `If-Modified-Since` present and
   `Date.parse(lastModified) <= Date.parse(ims)` → 304. 304 carries only the validator headers.
2. `Range` (only honoured for GET, and only if `If-Range` absent or matches: a quoted value is
   compared as etag ignoring `W/`, otherwise as an HTTP date equal to last-modified):
   regex `^bytes=\s*(\d*)\s*-\s*(\d*)\s*$` (case-insensitive) on the trimmed header. Both empty or
   no match → ignore range (200). Suffix `-N`: N==0 or size==0 → 416, else
   `[max(size-N,0), size-1]`. `S-E`/`S-`: `S > E` or `S >= size` → 416; else end clamped to
   `size-1`. Multi-range → no match → 200.
3. 416: empty body, `accept-ranges: bytes`, `content-range: bytes */<size>`.
4. 200/206: streamed bytes, `content-length`, `accept-ranges: bytes`, `content-type: <file.mime>`,
   206 adds `content-range: bytes <s>-<e>/<size>`.

`POST /api/experimental/fs/write?path=…` takes the raw body (`application/octet-stream`) and
returns the location envelope with `FileSystem.Write` (resolved absolute path).

---

## 9. PTY WebSocket protocols

Both PTY flavours use a single-writer outbound queue so replay, live data and the close frame
stay ordered; either side closing ends the connection and detaches
(`packages/server/src/handlers/pty-socket.ts:8-33`). Socket close/errors are swallowed.

### 9.0 Tickets (both flavours)

`POST …/connect-token` requires header `x-opencode-ticket: 1` (exact) **and** an allowed/same-host
`Origin` (absent Origin is allowed), else `403 ForbiddenError` (`handlers/pty.ts:119-143`,
`handlers/persistent-pty.ts:95-107`). The PTY must exist (404). Ticket = `crypto.randomUUID()`,
TTL 60 s, `expires_in: 60`, single use, in-memory cache capacity 10 000
(`packages/core/src/pty/ticket.ts:9-52`). Scope stored with the ticket:
`{ptyID, directory, workspaceID}` for location PTYs, `{ptyID}` for persistent PTYs; consumption
must match the stored scope exactly (ticket.ts:27-31). The `connect` URL with a non-empty
`ticket` param skips credential auth (§4.4); without a ticket normal auth applies
(`auth_token` query or cookie for browsers).

### 9.1 `GET /api/pty/{ptyID}/connect` (location PTY)

Handler `packages/server/src/handlers/pty.ts:144-218`; wire helpers
`packages/core/src/pty/protocol.ts`.

Query: `location[directory]` (or `x-opencode-directory` header; same as §3 — must match the
ticket's directory), `cursor`, `ticket`.

Pre-upgrade (plain HTTP responses, empty bodies):
1. Origin not allowed and not same-host → `403`.
2. `ticket` present and not consumable for `{ptyID, canonical(requestRef).directory, workspaceID}` → `403`.
3. Location build fails → `404` LocationNotFoundError JSON.
4. PTY does not exist → `404`.
5. `cursor`: if present and a safe integer `>= -1` it is used, otherwise ignored (full replay).

Upgrade to WebSocket, then outbound frames:
* Attach failure after upgrade: close `4404` reason `"session not found"` or `"session exited"`.
* Replay: retained output from `cursor` (omitted → whole retained buffer; `-1` → nothing,
  tail from current end), as **text** frames of at most 65 536 UTF-16 code units each
  (`REPLAY_CHUNK`, protocol.ts:13,23-27).
* Then one **binary** meta frame: byte `0x00` followed by UTF-8 JSON `{"cursor":<absolute cursor
  after replay>}` (protocol.ts:15-21).
* Then live output as **text** frames (one per PTY data chunk).
* When the PTY ends (exit, removal, teardown) → close `1000`.

Inbound frames: text → written as-is; binary → strict UTF-8 decode, invalid input dropped
(protocol.ts:30-37). There is no resize message on this socket (use `PUT /api/pty/{id}`).

Cursor units: the server buffer is a JS string; cursors/limits count UTF-16 code units and the
retained buffer is capped at 2 097 152 code units (`packages/core/src/pty.ts:12,52-70`). See Q10.

### 9.2 `GET /api/experimental/persistent-pty/{ptyID}/connect` (persistent PTY daemon)

Handler `packages/server/src/handlers/persistent-pty.ts:108-208`. Scope **G**.

Query: `cursor` (default `"0"`; must be a safe integer `>= 0` else `400` empty), `role`
(`observer` → observer, anything else → `controller`), `attachment_id` (default random UUID),
`takeover` (`"true"` only), `input_protocol` (`"1"` → framed input), `ticket`.
Pre-upgrade: origin check → `403`; ticket mismatch → `403`; bad cursor → `400`.

After the socket opens (attach happens in `onOpen`, persistent-pty.ts:128-182):
* Attach fails (not found / daemon unavailable) → close `4404` `"terminal unavailable"`.
* Text frame: `{"type":"attached","attachmentID","inputProtocol":0|1,"info":<PersistentPty.Info>,
  "role":"controller"|"observer","generation":n,"replay":{"requestedOffset","availableOffset",
  "endOffset","truncated"}}`.
* Binary frame with replay bytes (only if non-empty).
* Text frame `{"type":"replay_complete","endOffset":n}`.
* Live: `output` events → **binary** frames (raw bytes); `resized` →
  text `{"type":"resized","cols","rows","generation","checkpoint":"<base64>"}`; `exited`
  `{type,exitCode?,finalOffset}`, `controller_changed` `{type,attachmentID?,generation}`,
  `title_changed` `{type,title}`, `foreground_process_changed` `{type,process:string|null}` →
  text JSON (`packages/core/src/persistent-pty/daemon.ts:105-117`). End → close `1000`.

Inbound:
* `input_protocol` ≠ `1`: each message (text → UTF-8 bytes, binary as-is) →
  `PersistentPty.input(ptyID, attachmentID, info.size.cols, info.size.rows, bytes)` using the size
  captured at attach time.
* `input_protocol = 1`: binary frame `[type:u8][cols:u16 BE][rows:u16 BE][payload…]`; frames
  shorter than 5 bytes, `type ∉ {0,1}`, or `cols==0`/`rows==0` are ignored; `type 0` →
  `PersistentPty.control(id, attachmentID, cols, rows)` (take control / resize); `type 1` →
  `input(..., payload)`. Errors are ignored.

---

## 10. Server startup, discovery, web UI, health

### 10.1 Process & flags

`opencode serve [--hostname H] [--port P] [--cors ORIGIN…] [--service | --stdio]`
(`packages/cli/src/commands/handlers/serve.ts`, `packages/cli/src/server-process.ts:32-204`).

* Hostname default `127.0.0.1` (server-process.ts:67; process.ts:55).
* Port: explicit `P` binds exactly (0 = ephemeral). Otherwise (default/stdio mode) try 4096 and
  increment on `EADDRINUSE` up to 65535 (process.ts:142-149). Service mode default port
  (`packages/cli/src/services/service-config.ts:115-119`): `0xC0DE` (49374) for channels
  `latest|dev|beta|next`, `0xC0DF` (49375) for `local`, else `10000 + (parseInt(hash(channel)[0..8],16) % 50000)`; on Windows a port of `0x8000` or more becomes `0x7000 | (port & 0xfff)` (28894 for `latest`).
  Service-mode values may come from the service config file (`hostname`, `port`, `password`, `cors`).
* Stdout: default mode `server listening on http://H:P` then `server password <pw>` if the
  password was generated; stdio mode prints one JSON line `{"url":"http://H:P"}` and exits when
  stdin closes (server-process.ts:194-201).
* Server options plumbed from env: `OPENCODE_CONFIG_DIR`, `OPENCODE_CONFIG`,
  `OPENCODE_CONFIG_CONTENT`, `OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_MODELS_URL/PATH`,
  `OPENCODE_DISABLE_MODELS_FETCH`, `OPENCODE_FILEWATCHER_DISABLE`, `OPENCODE_DISABLE_FFF`,
  `OPENCODE_GIT_BASH_PATH`, `OPENCODE_SIMULATE`, `OPENCODE_PTY_HANDOFF` (server-process.ts:94-133).
  `ServerOptions` schema: `packages/server/src/options.ts:6-46`.

### 10.2 Boot sequence

`ServerProcess.start` (process.ts:47-140): bind the listener **first**, start serving the
dispatcher immediately, then build the application layer (database, services, routes) in the
background; on success set status `ready`; service mode also resumes sessions whose execution
claim was never released (process.ts:107-111, 235-242). On boot failure (service mode) status
becomes `failed` and the process keeps serving `/api/info` = 500 so supervisors can see it.

### 10.3 Health / identity — `GET /api/info`

The only health endpoint (old `/api/health`, `/api/server` were merged, V2_HTTP_API_AUDIT.md:54-58).
Always requires auth. Before ready the dispatcher answers (process.ts:188-191,203-217):
`{version, pid, urls, paths:{tmp}}` with `503` + `retry-after: 1` (starting/stopping) or `500`
(failed). After ready the HttpApi handler answers `200` with additionally
`capabilities:{persistentPty:boolean}`. Any other authenticated request before ready → 503
`{code:…}` (§6.3).

`urls` (`packages/server/src/server-info.ts:18-39`, process.ts:60-66): from the bound address;
if the requested hostname is `0.0.0.0` → `http://127.0.0.1:P` followed by every non-internal
IPv4 interface address; `::`/`[::]` → `http://[::1]:P` + non-internal IPv6 addresses
(bracketed); otherwise the single bound URL. Trailing `/` stripped, de-duplicated. Remote tunnel
URLs (service mode with `remote` configured: `https://<route>.<tunnel-host>`,
`packages/cli/src/services/remote-tunnel.ts`) are appended while connected
(packages/server/test/process.test.ts:240-262). `pid` is `process.pid` (0 on workerd). `version` = app version or
`"unknown"`.

### 10.4 Discovery

* **No mDNS / Bonjour / zeroconf** exists anywhere in the codebase (searched all packages).
* Service mode writes a registration file (`packages/cli/src/services/service-registration.ts:14-72`):
  path `<XDG_STATE_HOME or ~/.local/state>/opencode/service.json` (channels other than
  latest/dev/beta/next use `service-<channel>.json`, service-config.ts:110-113), content
  `{"id":<uuid>,"version","url":"http://H:P","pid","password"}`, mode `0600`, written to
  `<file>.<id>.tmp` then renamed. Every 5 s the server re-reads it; if it no longer matches
  (id, version, url, pid, password) or cannot be read, the server shuts down. On shutdown the file
  is removed if still owned.
* Clients (`packages/client/src/service-probe.ts:49-98`) read that file, `GET /api/info` with
  `Authorization: Basic base64("opencode:"+password)`: `404` ⇒ incompatible server; body `pid`
  must equal the file's pid (and `version` if present); `200` ready, `500` failed, other
  (503) waiting.

### 10.5 Web UI (`/`)

Provided by the CLI transform (`packages/cli/src/services/web-ui.ts`), executed **before**
dispatch/auth for any path other than `/api`, `/api/*`, `/auth/*`, `/openapi.json`:

* Assets: embedded archive `virtual:opencode-app-assets` = `[u32 LE indexLen][JSON index
  {"<path>":[offset,len]}][concatenated brotli-compressed bodies]`
  (`packages/cli/src/app-archive.ts:1-36`, `packages/cli/src/app-assets.ts`); local dev builds
  read `packages/app/dist` instead.
* Only `GET`/`HEAD` (else `405` empty). `HEAD` → 200 with headers, no body.
* Lookup key = pathname without leading `/`. Missing key under `_assets/` or `icons/` → `404`
  with `cache-control: no-store`; any other missing key → `index.html` (SPA fallback).
* Headers: `content-type` from extension; `cache-control: no-cache` for `index.html`, `sw.js`,
  `registerSW.js`, else `public, max-age=31536000, immutable`; `x-content-type-options: nosniff`;
  `content-security-policy` = `default-src 'self'; script-src 'self' 'wasm-unsafe-eval'[
  'sha256-<b64 sha256 of inline <script id="oc-theme-preload-script">>]; style-src 'self'
  'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self'
  data:; connect-src * data: blob:; worker-src 'self' blob:` (hash only for HTML)
  (web-ui.ts:83-92).
* If the request `Accept-Encoding` contains `br` (with q≠0) and the asset is not `index.html`, the
  stored brotli bytes are sent with `content-encoding: br` and `vary: accept-encoding`.
* `/auth/connect/{code}` browser success redirects to `/` (the SPA).

### 10.6 Compression

`HttpMiddleware.compression()` wraps everything (process.ts:73): responses are compressed per
`Accept-Encoding` (br observed in tests), except `text/event-stream` (not compressed) and bodies
already encoded. `fs.read` sets `no-transform` so Content-Length is preserved. Exact algorithm
list/threshold: Q5.


---

## 11. Open questions

1. **Effect version drift.** The checkout pins `effect@4.0.1` (root package.json catalog), but only
   `effect@4.0.0-beta.83` source was available locally for reading framework internals (SSE
   encoder, CORS, JSON codec, cause response). The SSE failure-event name in the checked-in
   OpenAPI/tests is `effect/http-api/stream/failure` (beta.83 uses `effect/httpapi/...`); other
   framework behaviours cited here (undefined↔null codec, CORS header set, 500-empty for defects,
   404-empty for unknown routes) should be re-verified against 4.0.1 or by black-box testing the TS
   server.
2. **Heartbeat timing.** `Stream.tick("15 seconds")` may emit its first tick immediately, so a
   `: heartbeat` comment could precede or follow the `server.connected` frame. Clients ignore
   comments; confirm whether ordering matters for any consumer.
3. **Excess/unknown request fields.** OpenAPI declares `additionalProperties:false`, but Effect
   decoding defaults to ignoring excess keys. Verify (send an extra key to the TS server) before
   deciding whether Rust should reject.
4. **Schema-error message text.** `InvalidRequestError.message` for decode failures is Effect's
   formatted `SchemaError` text; byte-exact parity is impractical. Proposal: same `_tag`/`kind`,
   best-effort message. Confirm no client parses it.
5. **Compression.** `HttpMiddleware.compression()` (process.ts:73) algorithm list, size threshold
   and content-type exclusions were not readable locally; tests only prove `br` for normal bodies
   and no compression for SSE.
6. **HEAD / method mismatch** on API routes (e.g. `HEAD /api/info`, `PUT /api/info`): does the
   Effect router auto-serve HEAD for GET and answer 404 vs 405? Needs black-box check.
7. **Event JSON is raw `JSON.stringify`** of the in-memory bus payload (event-feed.ts:29-31), not
   schema-encoded: `location` may include `workspaceID`, and any non-JSON-native values in `data`
   (DateTime, Option, branded classes) serialize via their `toJSON`. The Rust port needs the
   events spec to pin the exact JSON per event type; cross-check against a live capture.
8. **Empty/missing bodies** for endpoints whose body is entirely optional (e.g. `POST /api/pty`,
   `POST /api/session` with `{}`): is an absent body (no content-type) accepted or a 400 `Payload`
   error? The generated client always sends a JSON body when the schema has one.
9. **Request size limits** (body, header, URL) are not configured explicitly; Node/Effect defaults
   apply. Decide Rust limits (and 413 behaviour) — only `InstructionEntryValueTooLargeError` is a
   declared 413.
10. **PTY cursor units.** Location-PTY cursors, replay chunking (65 536) and the 2 Mi retention cap
    are measured in UTF-16 code units of a JS string (core/src/pty.ts:12). A Rust implementation
    storing bytes must convert to stay cursor-compatible with existing clients that persist
    cursors across reconnects.
11. **Location key normalisation.** `location[directory]` is used verbatim (no trailing-slash or
    symlink normalisation outside Windows). Replicate exactly, or normalise? Normalising changes
    `debug.location.list` output and cache identity.
12. **`/openapi.json` at runtime** is generated from the *server* Api (real middleware error
    sets, e.g. form endpoints) and may differ slightly from the checked-in client-side document.
    Is serving the checked-in file acceptable?
13. **Session-log SSE headers.** The Effect SSE response for `session.log` sets
    `content-type: text/event-stream`; whether it also sets cache/buffering headers (unlike the
    raw `/api/event` handler) is unverified.
14. **`pty.connect` OpenAPI shape** says `200 boolean` with `x-websocket: true`; any codegen in
    the Rust port must special-case `x-websocket` operations (101 upgrade) and `fs.read`
    (raw bytes) / `experimental.fs.write` (raw body).
15. **Audit/OpenAPI mismatch.** `V2_HTTP_API_AUDIT.md` lists 139 endpoints and does not cover
    `server.pair`, `server.connect`, `location.reload`, `credential.list`, `credential.create`,
    `integration.connect.external`, `experimental.fs.write` or `vcs.init` (all present in the
    generated OpenAPI's 142). The OpenAPI was treated as authoritative.
16. **Readiness 503 bodies** (`{code:"service_starting"}` etc.) have no `_tag` and are not in the
    OpenAPI; generated clients surface them as `UnexpectedStatus`. Keep verbatim.
17. **`server.connect` with `Accept: text/html` but JSON-capable clients**: browser detection is a
    substring test on `Accept` (`includes("text/html")`). Replicate exactly.
18. **Pairing / ticket caches** are in-memory with capacities 1 000 / 10 000; behaviour on
    overflow (LRU eviction in Effect `Cache`) should be mirrored or documented.

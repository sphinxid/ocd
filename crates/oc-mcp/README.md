# oc-mcp

MCP (Model Context Protocol) client for the opencode Rust port. Port of
`packages/core/src/mcp/**` (upstream v2) plus the parts of `@modelcontextprotocol/client@2.0.0`
upstream relies on (JSON-RPC client, Streamable HTTP transport, era negotiation, `auth()`).
Spec: `docs/spec/08-mcp-pty-plugins-misc.md` §1.

The JSON-RPC client is hand-rolled (no `rmcp`) so wire details, error strings and retry/expiry
behaviour match the TS SDK exactly.

## Modules

| module | upstream | contents |
|---|---|---|
| `config` | `schema/src/mcp.ts`, `config/plugin/mcp.ts`, `plugin/mcp-codemode-defaults.ts`, `v1/config/migrate.ts` | `ServerConfig` (`local`/`remote`), `TimeoutConfig` (defaults 30 000 / 30 000 / 43 200 000 ms), `Protocol`, `OAuthConfig`/`OAuthSetting` (`false` disables), `McpSection`, `merge_documents`, `apply_codemode_defaults`, `migrate_v1_server` |
| `types` | `schema/src/mcp.ts`, `mcp-event.ts` | `Status` (`connected`/`pending`/`disabled`/`failed{error}`/`needs_auth{error}`), `ServerInfo`, `Resource*`, `Tool`, `Prompt`, `PromptResult`, `CallToolContent`, `ToolResult`, `ClientInfo`, `McpEvent` |
| `manager` | `mcp/index.ts` | `McpManager` registry/lifecycle |
| `link` | `mcp/connection.ts` | `Link`: one server connection reconnected in place, with the tools and prompts it listed; reports `connecting`/`connected`/`failed{answered}`/`settled`/`closed`/`changed`/`legacy` to its owner (`LinkEvents`) under the owner's lock; per-endpoint handshake turns for unshared remote connections |
| `pool` | `mcp/pool.ts` | `McpPool` (process-wide, 2.2.6/2.2.7 `pool: true`): `key` (everything that changes a connection, secrets as fingerprints, the credential ID, the resolved local `cwd`), shared slots with holders, the `unpoolable`/`legacy` verdicts, `Pooled` (a Location's connection: joins the slot or connects privately), first-answer-wins elicitations no request claims |
| `connection` | `mcp/client.ts` | one connection: transport choice, `?codemode=false`, transient retry, session expiry, teardown; `ConnectError::Connect{answered}` (`!repeats(error)`), `roots: false` for shared connections, per-request elicitation handlers (`call_tool_for`, `prompt_for`, `read_resource_for`) |
| `protocol` | SDK `Protocol`/`Client` | request correlation, timeouts, cancellation, legacy handshake, `2026-07-28` negotiation (an unanswered or closed probe on stdio means a legacy server or the SDK's "probed in place" failure), server requests, pagination; results are not checked against a tool's `outputSchema` (2.2.7) |
| `tool_check` | `mcp/client.ts` `tools()` (2.2.7) | per-tool checks of `tools/list`: a tool that is not an object, has no non-empty name, no object `inputSchema` or an `inputSchema.type` other than `"object"` is skipped with the warning "skipping invalid MCP tool" `{server, tool, reason}`; the rest are copied keeping only fields of the right type (`inputSchema` with `type:"object"`, `properties` if a record, `required` if all strings; `icons` dropped) |
| `transport::stdio` | `mcp/stdio.ts` | process-group spawn, newline framing (no overall buffer cap; a line over 16 MiB reports `MCP server {server} wrote more than 16 MiB to stdout without a message boundary` and closes the transport; blank lines are skipped and invalid ones reported as `Invalid MCP stdio message from {server}: {cause}`), stderr tail, kill sequence |
| `transport::http` | SDK `StreamableHTTPClientTransport` | POST JSON/SSE, `Mcp-Session-Id`, GET stream, SSE resumption/reconnect, the auth provider's `token()`/`onUnauthorized` on 401 (one retry; `Server returned 401 after re-authentication` after it), a 403 `insufficient_scope` as `InsufficientScopeError` (needs_auth, no step-up from a connection, 2.3.0), DELETE |
| `oauth` | `mcp/oauth.ts`, SDK `auth.ts` | the login's provider, discovery (RFC 9728/8414/OIDC), DCR (with `logo_uri`), CIMD, PKCE, credential mapping (with the login's `metadata.discovery`), `McpCredentialStore`, pasted redirect URLs (`Submitter`) |
| `oauth::connect` | `mcp/oauth.ts` `connectProvider` (2.3.0) | `ConnectAuth`, a connection's `AuthProvider` over the credential the manager selected (the integration's last row), or anonymous without one (a 401 is `Unauthorized`, no OAuth request). `token()` refreshes a token that expires within 30 s first when the login saved its discovery (on failure the current token goes out); `on_unauthorized` refreshes while the row still holds the refused token. A rejected refresh (`invalid_grant`, `invalid_client`, `unauthorized_client`, no client information) removes the credential and refuses with `MCP server "<url>" requires user authorization`; an issuer mismatch refuses and keeps it; anything else fails the attempt with `temporarily_unavailable` (`Could not refresh the OAuth token: <code or message>`, never the server's body) |
| `oauth::refresh` | `credential.ts` `Credential.refresh`, `mcp/oauth.ts` `exchange` | a stored credential's refresh runs under the in-process share plus the file lock `credential-refresh:<id>`, re-reads the row (`stale` false: another caller refreshed it, its row is returned), has a 30 s limit including the body, discards its result when the row changed meanwhile. `exchange` refreshes with the saved discovery (`metadata.discovery`), else discovers as the login did (following the 401's `resource_metadata`) and saves it; a configured `auth_server_metadata_url` is read on every refresh and saves none. Discovery naming another issuer than `metadata.issuer` sends nothing; token-endpoint 429/5xx become `HTTP <status>` |
| `oauth::callback`, `oauth::page` | `mcp/oauth.ts` `authorize`, `oauth/page.ts` | loopback callback server and HTML pages; both `http://127.0.0.1:<port><path>` and `http://localhost:<port><path>` are registered (unless `redirect_uri` is configured) and the code is redeemed with the one whose `Host` the browser came back on (#63403) |
| `naming` | `tool/mcp.ts`, `plugin/command.ts`, `tool/plugin/mcp-resource.ts`, `mcp/instructions.ts` | `<server>_<tool>` naming, tool output mapping, prompt slash commands, resource-tool strings, `<mcp_instructions>` rendering |
| `cli` | `cli/.../handlers/mcp/*.ts` | `mcp list/add/auth/logout` strings and `format_list` |

## McpManager

```rust
let manager = McpManager::new(ManagerOptions {
    directory,                                   // Location directory (stdio cwd base, roots/list)
    client_info: ClientInfo { name: "opencode".into(), version: app_version },
    credentials: Some(store),                    // Arc<dyn McpCredentialStore>, over the SQLite `credential` table
    elicitation: Some(handler),                  // Arc<dyn ElicitationHandler>
    pool: Some(pool),                            // McpPool shared by the process's Locations (core: App::mcp_pool)
});
let mut events = manager.subscribe();            // broadcast::Receiver<McpEvent>
manager.on_event(sink);                          // or a synchronous EventSink, called as each event is published

// Config: fold every document's `mcp` section, drop policy-denied servers, then load.
let servers = oc_mcp::merge_documents(sections.iter());
manager.set_servers(servers).await;              // initial load and every `config.updated` reload
```

`set_servers` applies the built-in codemode defaults and reconciles exactly like upstream: the
first call creates all entries `pending`, marks `disabled` ones and starts the rest in the
background; later calls replace changed servers (deep equality), remove missing ones and leave
the rest untouched. Runtime `add` overrides win over config; `remove` hides a name until restart.
The first call's `mcp.status.changed` events (each disabled server, then each starting one) reach the
sink before it returns, as upstream's forked startups announce `pending` before the MCP State's
notification returns; core publishes them on the bus through the sink.

| method | behaviour |
|---|---|
| `servers() -> Vec<ServerInfo>` | sorted by name; `title` = the connected server's trimmed `serverInfo.title` (absent when empty or disconnected); `integrationID` set for OAuth-capable remotes |
| `status(name)`, `integration_id(name)`, `wait_ready(name)` | current status / OAuth integration id (`mcp_` + sha1(name\0url)[..16]) / await first connect attempt |
| `await_startup()`, `flush()` | `awaitStartup` / `mcpTools.flush`: wait for servers still connecting, each until it settles or 2 s after its attempt began; `flush` then brings `registered_tools()` up to date |
| `add(name, config)` | runtime add/replace, connects immediately (not persisted) |
| `connect(name)` / `disconnect(name)` / `remove(name)` | stop+start (overrides `disabled`) / stop + `disabled` / remove until restart |
| `tools() -> Vec<Tool>` | connected tools sorted by server, name; `Tool::prefixed_name()` = `namespace(server)_sanitize(tool)`; `registry_input()`/`registry_output()` give the registry schemas |
| `registered_tools() -> Vec<Tool>` | what the session tool registry lists (`tool/mcp.ts`): `tools()` read when first needed, then again only once `mcp.tools.changed` events settled for 100 ms (`TOOLS_DEBOUNCE`), so a step right after a server went away still lists its tools |
| `call_tool(server, tool, args, session_id) -> Result<ToolResult, ManagerError>` | waits for startup; unavailable/failed messages verbatim (`MCP server "<s>" is not connected: <e>. Reconnect it from /mcps.` …, `MCP tool "<t>" on server "<s>" failed: <msg>`); `naming::tool_output` maps the result to the tool output (`isError` → failure text) |
| `instructions() -> Vec<(server, text)>` | trimmed, non-empty, sorted |
| `prompts()`, `prompt(server, name, args)` | prompt list; `prompts/get` (`None` on any failure) |
| `resource_catalog()`, `resources(server)`, `read_resource(server, uri)` | merged/sorted catalog (template-list failures swallowed); read returns `None` without the `resources` capability |
| `auth_start(server) -> AuthorizeHandle` | interactive OAuth: `handle.url`, `handle.instructions` (`Authorize <name> in your browser. This window will close automatically.`), `handle.mode == "auto"`, `handle.wait().await -> Result<credential JSON, String>`, `handle.canceller()` (cancel to close the callback server). Core registers each remote server as an `mcp_<hash>` OAuth integration whose method runs this flow (`oc_core::mcp::register_integrations`) |
| `auth_finish(server, credential) -> credential id` | stores the credential (keeps previous label, removes old rows), reconnects |
| `logout(server) -> usize` | removes all stored credentials of the integration, reconnects |
| `credential_switched(integration_id)` | call when core's credential store switches a credential (`Credential.Event.Switched`) |
| pooling | a server with `pool: true` (and a protocol other than `legacy`) connects through the pool with `protocol: "auto"` when unset. Its start joins the shared connection for its key (`connecting`, then `connected` or the shared failure), or, when the key can't be shared, connects on its own (a key that negotiated legacy skips the probe). Logs: `mcp pool ignored: server speaks an older protocol` `{server, version}`, `mcp pool ignored: server refused the shared connection` `{server, error}`, once per name `mcp pool ignored: local server has no fixed cwd, so each directory runs its own` `{server}`. A shared connection closing on its own fails every holder; `disconnect`/`shutdown` lets go, and the last holder closes it |
| `shutdown()` | stops every server (kills stdio processes, DELETEs HTTP sessions); from its start nothing is published and connection callbacks no longer run (#60668) |

Events (`McpEvent::event_type()`, payload `{server}`): `mcp.tools.changed`,
`mcp.resources.changed`, `mcp.status.changed`, `mcp.prompts.changed`.

Errors: `ManagerError::NotFound` (`MCP server not found: <name>`), `ManagerError::ToolCall`,
`ManagerError::Other`.

## McpCredentialStore

Core implements this over the global SQLite `credential` table:

```rust
#[async_trait]
pub trait McpCredentialStore: Send + Sync {
    async fn list(&self, integration_id: &str) -> anyhow::Result<Vec<StoredCredential>>; // list order; MCP uses the last
    async fn get(&self, id: &str) -> anyhow::Result<Option<StoredCredential>>;
    async fn create(&self, integration_id: &str, label: &str, value: Value) -> anyhow::Result<String>;
    async fn update(&self, id: &str, value: Value) -> anyhow::Result<()>;
    async fn remove(&self, id: &str) -> anyhow::Result<()>;
}
pub struct StoredCredential { pub id: String, pub integration_id: String, pub label: String, pub value: Value }
```

`value` for MCP OAuth (`Credential.OAuth`, key order as written):

```json
{ "type": "oauth", "methodID": "mcp_<16hex>",
  "access": "<access_token>", "refresh": "<refresh_token or ''>",
  "expires": 1730000000000,
  "metadata": { "serverUrl": "<config url>", "tokenType": "Bearer",
                "scope": "...", "issuer": "...",
                "discovery": { "authorizationServer": "...", "resource": "..." },
                "client": { "client_id": "...", "client_secret": "...", "issuer": "...", "...": "..." } } }
```

`expires` is epoch ms (`now + expires_in*1000`), `0` = non-expiring; `scope`, `issuer`, `discovery` and
`client` are optional. `discovery` is where the login found its authorization server and which resource it named
(absent when `auth_server_metadata_url` is configured). Connect-time refreshes go through `oauth::connect` and
`oauth::refresh` (the file lock lives in `McpCredentialStore::lock_root()`, core's `<state>/locks`) and rewrite the
row with the refreshed tokens, the issuer discovery found, the discovery used and the stored `client`; a rejected
refresh removes the row. `MemoryCredentialStore` is an in-memory implementation.

## ElicitationHandler

```rust
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn create(&self, server: &str, params: Value, cancel: CancellationToken) -> Result<Value, String>;
    async fn complete(&self, server: &str, elicitation_id: &str);
}
```

`params` is the `elicitation/create` params with `mode` defaulted to `"form"`; return
`{"action": "accept"|"decline"|"cancel", "content"?}`. Form defaults from `requestedSchema` are
applied to accepted content. Core maps these to `"global"`-session forms titled
`<server> is requesting input`.

## Not implemented / differences

- Modern era (`protocol: "auto"` / `"2026-07-28"`): probe, envelope, `resultType` decoding,
  `input_required` auto-fulfilment and the auto `subscriptions/listen` are implemented; the SDK
  response cache, `Mcp-Param-*` header mirroring and the stdio "disposable sibling" probe are not
  (upstream's custom stdio transport does not use the sibling probe either). A tool call that a modern server
  rejects with `-32020` (HeaderMismatch) is relisted and retried once, as upstream, though without the mirroring
  ocd never causes one.
- The opt-in connection pool (`pool: true`, 2.2.7) is not implemented: each Location connects on its own.
- Sampling (`sampling/createMessage`) is not supported (upstream does not declare it).
- `tools/list` pages are not validated as a whole (a page that fails for another reason than its tools is accepted,
  where the SDK fails it, e.g. a modern page without `ttlMs`).
- Network error messages approximate Bun's fetch errors (`Unable to connect. Is the computer able to access the url? (ConnectionRefused)`).
- Workspace-backed (remote environment) stdio spawning is core's concern; this crate spawns locally.
- Legacy v1 config normalization diagnostics live in core; `migrate_v1_server` covers the server mapping.

## Tests

`cargo test -p oc-mcp`: unit tests per module, `tests/stdio.rs` (the test binary re-executed as a
fake stdio server) and `tests/http.rs` (axum fake Streamable HTTP server + OAuth AS).

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
| `connection` | `mcp/client.ts` | one connection: transport choice, `?codemode=false`, transient retry, session expiry, teardown |
| `protocol` | SDK `Protocol`/`Client` | request correlation, timeouts, cancellation, legacy handshake, `2026-07-28` negotiation, server requests, pagination, output-schema validation |
| `transport::stdio` | `mcp/stdio.ts` | process-group spawn, newline framing (16 MiB frame cap), stderr tail, kill sequence |
| `transport::http` | SDK `StreamableHTTPClientTransport` | POST JSON/SSE, `Mcp-Session-Id`, GET stream, SSE resumption/reconnect, 401/403 OAuth, DELETE |
| `oauth` | `mcp/oauth.ts`, SDK `auth.ts` | provider, discovery (RFC 9728/8414/OIDC), DCR, CIMD, PKCE, refresh (de-duplicated), credential mapping, `McpCredentialStore` |
| `oauth::callback`, `oauth::page` | `mcp/oauth.ts` `authorize`, `oauth/page.ts` | loopback callback server and HTML pages |
| `naming` | `tool/mcp.ts`, `plugin/command.ts`, `tool/plugin/mcp-resource.ts`, `mcp/instructions.ts` | `<server>_<tool>` naming, tool output mapping, prompt slash commands, resource-tool strings, `<mcp_instructions>` rendering |
| `cli` | `cli/.../handlers/mcp/*.ts` | `mcp list/add/auth/logout` strings and `format_list` |

## McpManager

```rust
let manager = McpManager::new(ManagerOptions {
    directory,                                   // Location directory (stdio cwd base, roots/list)
    client_info: ClientInfo { name: "opencode".into(), version: app_version },
    credentials: Some(store),                    // Arc<dyn McpCredentialStore>, over the SQLite `credential` table
    elicitation: Some(handler),                  // Arc<dyn ElicitationHandler>
});
let mut events = manager.subscribe();            // broadcast::Receiver<McpEvent>

// Config: fold every document's `mcp` section, drop policy-denied servers, then load.
let servers = oc_mcp::merge_documents(sections.iter());
manager.set_servers(servers).await;              // initial load and every `config.updated` reload
```

`set_servers` applies the built-in codemode defaults and reconciles exactly like upstream: the
first call creates all entries `pending`, marks `disabled` ones and starts the rest in the
background; later calls replace changed servers (deep equality), remove missing ones and leave
the rest untouched. Runtime `add` overrides win over config; `remove` hides a name until restart.

| method | behaviour |
|---|---|
| `servers() -> Vec<ServerInfo>` | sorted by name; `title` = the connected server's trimmed `serverInfo.title` (absent when empty or disconnected); `integrationID` set for OAuth-capable remotes |
| `status(name)`, `integration_id(name)`, `wait_ready(name)` | current status / OAuth integration id (`mcp_` + sha1(name\0url)[..16]) / await first connect attempt |
| `add(name, config)` | runtime add/replace, connects immediately (not persisted) |
| `connect(name)` / `disconnect(name)` / `remove(name)` | stop+start (overrides `disabled`) / stop + `disabled` / remove until restart |
| `tools() -> Vec<Tool>` | connected tools sorted by server, name; `Tool::prefixed_name()` = `namespace(server)_sanitize(tool)`; `registry_input()`/`registry_output()` give the registry schemas |
| `call_tool(server, tool, args, session_id) -> Result<ToolResult, ManagerError>` | waits for startup; unavailable/failed messages verbatim (`MCP server "<s>" is not connected: <e>. Reconnect it from /mcps.` …, `MCP tool "<t>" on server "<s>" failed: <msg>`); `naming::tool_output` maps the result to the tool output (`isError` → failure text) |
| `instructions() -> Vec<(server, text)>` | trimmed, non-empty, sorted |
| `prompts()`, `prompt(server, name, args)` | prompt list; `prompts/get` (`None` on any failure) |
| `resource_catalog()`, `resources(server)`, `read_resource(server, uri)` | merged/sorted catalog (template-list failures swallowed); read returns `None` without the `resources` capability |
| `auth_start(server) -> AuthorizeHandle` | interactive OAuth: `handle.url`, `handle.instructions` (`Authorize <name> in your browser. This window will close automatically.`), `handle.mode == "auto"`, `handle.wait().await -> Result<credential JSON, String>`, `handle.canceller()` (cancel to close the callback server). Core registers each remote server as an `mcp_<hash>` OAuth integration whose method runs this flow (`oc_core::mcp::register_integrations`) |
| `auth_finish(server, credential) -> credential id` | stores the credential (keeps previous label, removes old rows), reconnects |
| `logout(server) -> usize` | removes all stored credentials of the integration, reconnects |
| `credential_switched(integration_id)` | call when core's credential store switches a credential (`Credential.Event.Switched`) |
| `shutdown()` | stops every server (kills stdio processes, DELETEs HTTP sessions) |

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
                "client": { "client_id": "...", "client_secret": "...", "issuer": "...", "...": "..." } } }
```

`expires` is epoch ms (`now + expires_in*1000`), `0` = non-expiring; `scope`, `issuer` and
`client` are optional. Connect-time refreshes rewrite the row (`update`) keeping `client`;
`invalid_grant` deletes the row only while it still holds the refresh token this connection
presented. `MemoryCredentialStore` is an in-memory implementation.

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
  (upstream's custom stdio transport does not use the sibling probe either).
- Sampling (`sampling/createMessage`) is not supported (upstream does not declare it).
- JSON-Schema output validation uses the `jsonschema` crate; its error texts differ from Ajv's.
- Network error messages approximate Bun's fetch errors (`Unable to connect. Is the computer able to access the url? (ConnectionRefused)`).
- Workspace-backed (remote environment) stdio spawning is core's concern; this crate spawns locally.
- Legacy v1 config normalization diagnostics live in core; `migrate_v1_server` covers the server mapping.

## Tests

`cargo test -p oc-mcp`: unit tests per module, `tests/stdio.rs` (the test binary re-executed as a
fake stdio server) and `tests/http.rs` (axum fake Streamable HTTP server + OAuth AS).

# oc-plugin: server plugins and the AI SDK bridge in a Bun sidecar

Runs upstream opencode v2 **server plugins**, written against the public `@opencode/plugin` API in
Promise or Effect style, unchanged (phase 1). They run in a Bun sidecar that ocd starts only when a
location's config needs a package or local plugin. The sidecar talks JSON-RPC to the Rust server
over stdio.

The same sidecar runs **AI SDK provider packages** (phase 2): models whose package is an `aisdk:`
npm AI SDK provider without a native Rust implementation, with the plugin `aisdk.sdk` /
`aisdk.language` hooks. See "AI SDK bridge" below; the sidecar starts on the first such model even
when no plugin is configured.

What is out of scope:

- **TUI plugins** (the `tui` entrypoint) are not supported. `@opencode/plugin/tui*` is not provided
  to plugins.

Upstream references used throughout: `packages/plugin/src/*` (public API, adapter, host, runtime)
and `packages/core/src/plugin/{host,hooks,module,supervisor}.ts`, `packages/core/src/plugin.ts`,
`packages/core/src/rpc.ts` and `packages/util/src/npm.ts` at commit `4617210822bd`.

## Architecture

```
          oc-core (implements PluginApi)                     Bun 1.4.2 sidecar (host/dist/host.js)
 ┌───────────────────────────────────────┐   stdin  ┌──────────────────────────────────────────────┐
 │ PluginHost                            │ ───────▶ │ upstream @opencode/plugin + effect (bundled)  │
 │  load / trigger_hook / transform /    │          │ ensurePluginRuntime(): plugins share them     │
 │  execute_tool / invoke / rpc_call /   │ ◀─────── │ loader: Host.resolve, createPluginSources,    │
 │  publish_event                        │  stdout  │   Module validation, fromPromise adapter      │
 │ mirrors: hooks, transform domains,    │          │ LocationRuntime per location: activation,     │
 │  domain revisions, rpc ids, inventory │ ◀─────── │   hooks, transforms (editors), RPC registry,  │
 │ npm cache (bun add), Bun download     │  stderr  │   $fn handles, event PubSub                   │
 └───────────────────────────────────────┘  (logs)  │ ctx = proxy: API calls → `api` requests       │
                                                     └──────────────────────────────────────────────┘
```

- **The plugin-facing code is upstream's own.** The bundle embeds `@opencode/plugin` (the Promise →
  Effect adapter `fromPromise`, `Host.resolve`/`Host.load`, `createPluginSources`, `runtime.ts`), the
  schema and protocol packages, `effect` 4, and `packages/core/src/tool/runtime.ts` (tool input
  validation, output encoding and content normalisation). Only the *host side* of `ctx` is new
  (`host/src/context.ts`). It is an Effect `Plugin.Context` whose members forward to Rust, so Promise
  plugins see exactly what upstream's adapter makes of it.
- **Shared module instances.** `ensurePluginRuntime()` is upstream's `runtime.ts`. The build step
  replaces `runtime-modules.ts` with a static map, the way upstream's CLI build does, so
  `import "effect"` or `"@opencode/plugin/..."` in a plugin resolves to the bundle's instances. A
  copy found in a plugin's `node_modules` throws upstream's
  `Cannot load "<x>" from plugin node_modules: ...`.
- **Rust owns all state.** Domains (tools, agents, commands...) live in the core. The sidecar keeps
  only the plugins' registrations and runs plugin callbacks when Rust asks it to.
- **Functions never cross the boundary as code.** A function or lazy Effect value crosses as
  `{"$fn": "<handle>"}`.
  - Handles minted by the sidecar start with `js:`. The core calls them through `invoke`, or through
    `execute_tool` for tool executors.
  - All other handles belong to Rust. The core may put `{"$fn": "rs:..."}` into a transform value,
    for example a native tool's `execute`. If a plugin calls it, the sidecar calls back through
    `PluginApi::invoke`. If the entry comes back with the same handle, the plugin did not replace
    the function, so the entry is still native.
- **One sidecar for all locations.** Each location (directory plus workspace) gets its own
  `LocationRuntime` with its own plugin instances, matching upstream's per-location plugin host.
  Module imports are shared within the process.

### Lifecycle

- **Lazy start.** `PluginHost::load` resolves operations with upstream's supervisor rules, in
  `src/supervisor.rs`. The sidecar starts only when an add operation needs a package or local plugin.
  `bun` is resolved by `src/bun.rs`, and the embedded bundle is written to
  `<Global.bin>/ocd-plugin-host/<sha16>/host.js`.
- **Load and activate.** Every add operation that needs a package or local plugin is loaded with
  `module.load`. npm packages are first resolved or installed by `src/npm.rs`. Then
  `location.activate` runs upstream's activation algorithm:
  - plugins whose (id, revision) prefix is unchanged stay alive;
  - the rest are torn down in reverse order and set up again;
  - a failed new revision falls back to the previous activation.
- **Hot reload.** Local plugins are fingerprinted by upstream's `createPluginSources`. The sidecar
  reports every file to watch through `HostEvent::Watch`. When one changes, the core calls `load`
  again, and the new revision is re-imported.
- **Crash recovery.** If the sidecar dies, the host emits `HostEvent::Crashed`. It then restarts the
  sidecar after a backoff of 1, 2, 4, 8, 16, then 30 s. The backoff resets once the process has lived
  for more than 60 s. Every location's last `load` is replayed, then `HostEvent::Restarted` is
  emitted, and the core should rebuild all plugin domains.
- **Shutdown.** `PluginHost::shutdown` sends `shutdown`, which closes every location: plugin scopes
  close and Promise cleanup functions run. Closing the sidecar's stdin has the same effect. After 5 s
  the process is killed. The child is also `kill_on_drop`.
- **Logs.** The sidecar writes JSON lines to stderr, `{"$log": level, "message", ...fields}`, which
  become `tracing` events with target `oc_plugin::host`. Plugin `Effect.log*` calls are routed there
  too, tagged `plugin: <id>`. Plugin `console.*` output is redirected to stderr, and any non-JSON
  stdout or stderr line is logged under target `oc_plugin::plugin`.

## Protocol

The protocol is newline-delimited JSON-RPC 2.0 (`src/protocol.rs`, `host/src/peer.ts`).

- **Message kinds.** Both sides send requests, and ids are scoped per direction. A message with
  `method` and `id` is a request, `method` alone is a notification, and `id` alone is a response.
- **Errors** have the shape `{message, name?, type?, data?, metadata?}`.
- **Cancellation.** `$/cancel {id}` aborts an in-flight request in either direction. Dropping a Rust
  request future sends it.
- **Locations** are named by `location`, which is `LocationRef::key()`: the directory, plus `\0`
  and the workspace ID when there is one.

### Rust → sidecar

| method | params | result |
|---|---|---|
| `initialize` | `{app, protocol}` | `{protocol: 1, bun, pid}` |
| `module.load` | `{operation: {type:"add", target, options, mtime?}, install?: {directory, name, version?, revision?}}` | `{id, revision, features, source}` or `{error, ref}` |
| `location.activate` | `{location, ref, info, plugins: [{target, revision}], failures}` | `{inventory, hooks, domains, rpc}` |
| `location.close` / `location.list` | `{location}` | `null` / inventory |
| `hook.trigger` | `{location, key, event}` | `{event}`; or an error with `type` `tool.error` (+`metadata`) or `defect` |
| `transform` | `{location, domain, value}` | `{value}` |
| `tool.execute` | `{location, handle, input, context}` | `{output?, content, metadata?}`; or an error with `type` `tool.error` or `defect` |
| `invoke` | `{location, handle, args, scope?}` | JSON result (returned functions become new `js:` handles) |
| `scope.close` | `{location, scope}` | `null` |
| `rpc.call` | `{location, rpcID, method, input}` | `{output}`; or an error with `type` = RPC failure type, plus `data` |
| `shutdown` | `{}` | `null`, then the sidecar exits |
| `event` (notification) | `{location?, event}` | — |
| `aisdk.load` | `{location, model, enabled?}` | `{}`; or an error with `type` `aisdk.init` and `data.detail` (upstream `causeMessage`) |
| `aisdk.stream` | `{location, model, options, stream, http?: {response}, enabled?}` | `null` after the last part; or an error with `type` `aisdk.error` and `data: {operation, shape}` |
| `aisdk.close` | `{location}` | `null` (drops the location's language model and SDK caches) |

### Sidecar → Rust

| method | params | handled by |
|---|---|---|
| `api` (request) | `{location, plugin, method, input}` | `storage.*` → `PluginApi::storage_*` with namespaced keys; everything else → `PluginApi::call` |
| `invoke` (request) | `{location, handle, args}` | `PluginApi::invoke` |
| `domains.changed` | `{location, domains, active}` | bumps `domain_revision`, emits `HostEvent::DomainsChanged` |
| `hooks.changed` | `{location, hooks: [{key, providerID?}]}` | mirror for `has_hook`, emits `HooksChanged` |
| `rpc.changed` | `{location, ids}` | mirror for `rpc_ids`, emits `RpcChanged` |
| `plugins.changed` | `{location, inventory}` | mirror for `packages`, emits `PluginsChanged` |
| `events.interest` | `{location, count}` | `publish_event` is a no-op while every count is 0 |
| `tool.progress` | `{location, id, metadata}` | the `progress` callback passed to `execute_tool` |
| `watch` | `{file}` | emits `HostEvent::Watch` |
| `aisdk.part` (notification) | `{stream, part}` | the stream of [`PluginHost::aisdk_stream`] |
| `aisdk.http.request` / `aisdk.http.response` (requests) | `{stream, request[, response]}` | the stream's `AisdkHttp` (`session.http.*` hooks) |
| `npm.add` (request) | `{spec}` | `Npm::add` in the plugin npm cache; answers `Installed` |

The sidecar flushes pending change notifications before it sends any `api` request, and Rust
applies notifications in arrival order. Example: a plugin that registers a transform and then calls
`ctx.tool.list()` during setup. By the time that call reaches Rust, `domain_revision` already shows
the change.

## ctx surface

Legend: **S** = supported; **S\*** = supported with a noted difference; **U** = unsupported.
Each "api" item becomes a `PluginApi::call` with the listed method. The full method contract,
with HTTP endpoints, is in `src/api.rs`.

| ctx member | status | how |
|---|---|---|
| `app`, `location`, `options` | S | `app` from `HostOptions.app`, `location` = `LoadRequest.info` decoded as `Location.Info`, `options` from the config entry |
| `agent.get` / `list` | S | api `agent.get` / `agent.list` (`location` query passes through) |
| `agent.transform` / `reload` | S | transform domain `agent`; api `agent.reload` |
| `aisdk.hook("sdk" \| "language")` | S | registered in the location's AI SDK service in the sidecar (`host/src/aisdk.ts`), after the internal provider plugins' hooks; `providerID` scoping and the copy-back of `sdk` / `language` as in `core/src/plugin/host.ts:157` |
| `command.list` | S | api `command.list` |
| `command.transform` / `reload` | S | transform domain `command`; commands carry `execute` handles |
| `event.subscribe()` | S | `publish_event` → `event` notification; decoded with `OpenCodeEvent`, filtered to server + `rpc.*` events |
| `experimental.terminal.read` | S | api `persistentPty.read` |
| `generate.text` | S | api `generate.text` |
| `integration.list/get/connect.*/oauth.*/command.*` | S | api `integration.*` |
| `integration.connection.active/resolve/status` | S | api `integration.connection.*` (raw JSON) |
| `integration.transform` / `reload` | S\* | domain `integration`; OAuth `authorize/refresh/label` are handles and the core drives the flow (use `invoke` with a `scope`) |
| `mcp.list`, `mcp.transform` / `reload` | S | api `mcp.list`; domain `mcp` |
| `model.list/default`, `model.transform` / `reload` | S\* | domain `model`; `AISDKNative.rewrite` is not applied in the sidecar (the core normalises) |
| `permission.list/get/reply` | S | api `session.permission.*` |
| `permission.hook("evaluate")` | S | hook `permission.evaluate` |
| `plugin.list` | S | api `plugin.list` (the core answers with the full inventory) |
| `provider.list/get`, `provider.transform` / `reload` | S\* | domain `provider`; no `AISDKNative.rewrite` (as for `model`) |
| `reference.list`, `reference.transform` / `reload` | S | api `reference.list`; domain `reference` |
| `rpc(definition)` client | S | local RPCs run in the sidecar; others go to api `rpc.call`; `events.subscribe` uses the forwarded event stream |
| `rpc.register` | S | registry in the sidecar (port of `core/src/rpc.ts`, same failure types and messages); `events.emit` → api `event.publish` |
| `session.create/get/remove/switchAgent/switchModel/prompt/generate/command/compact/synthetic/interrupt/update/move/wait/context` | S | api `session.*` |
| `session.hook(...)` | S\* | see the hook table below |
| `shell.hook("create.before")` | S | hook `shell.create.before` |
| `skill.list`, `skill.transform` / `reload` | S | api `skill.list`; domain `skill` |
| `storage.get/set/remove/scan` | S | `PluginApi::storage_*`; keys `plugin:<utf16 hex>:<key>` |
| `tool.transform` / `reload` / `list` | S | domain `tool`; api `tool.list` returns the current tool value |
| `tool.hook("execute.before" \| "execute.after")` | S | hooks `tool.execute.*` |
| `vcs.get/base/branch.list/status/diff`, `vcs.transform` / `reload` | S | api `vcs.*`; domain `vcs` (definitions carry handles) |
| `websearch.providers/query`, `websearch.transform` / `reload` | S | api `websearch.*`; domain `websearch` |
| `worktree.list/create/refresh/remove`, `worktree.transform` / `reload` | S | api `worktree.*`; domain `worktree` |
| TUI entrypoint, `@opencode/plugin/tui` | U | out of scope; `features.tui` is still reported |

Behavior that matches upstream verbatim:

- The validation message `Plugin must export a default definition with an id and an effect or
  setup function.`
- `Plugin entrypoint not found: <target>`.
- `Plugin failed to load` for any other load error, with a `ref` of `err_<8 hex>`.
- `Duplicate plugin ID: <id>`.
- `Plugin disabled after <domain>.transform failed. Check server logs for details.`
- Tool input errors: `Invalid arguments for tool "<name>": ...`.
- Tool registration errors: `Invalid tool name`, the reserved name `execute`, and invalid namespace.
  These are returned in `errors` and logged as `Skipping invalid tool registration`.

## Hooks: where the core must call `trigger_hook`

At each site, run the native (internal-plugin) logic in upstream's order around the plugin call:
the internal plugins in `pre` before it, and those in `post` after it. Pass the event as plain JSON
with upstream's field names. Use the returned event.

`trigger_hook` costs no IPC when no plugin registered the key. `has_hook(location, key,
providerID)` matches upstream `PluginHooks.has`.

Callbacks scoped by `providerID` are filtered in the sidecar using `event.model.providerID`.

| key | event (mutable fields **bold**) | upstream site | ocd site | status |
|---|---|---|---|---|
| `session.prompt` | `sessionID, messageID, `**`prompt`**`{text, files?, agents?, skills?}, `**`metadata?`**`, `**`delivery`** | `packages/core/src/session/prompt.ts:40` | `oc-backend/src/sessions.rs` `prompt`, after awaiting activation and before attachments are materialised | wired |
| `session.context` | `sessionID, model{id,providerID,variant?}, agent, `**`system, messages, options, tools{name:{description,input}}`** | `session/model-request.ts:422` (`primary`) | `oc-core/src/plugin_request.rs` `shape`, from `runner.rs` `run_step`; renamed/removed tools map back in `LocalTools` | wired |
| `session.compaction` | as context + **`result?`** (set → skip the model call) | `session/model-request.ts:422` via `:426` | `compaction.rs` `Run::stream` | wired |
| `session.generate` | as context | `session/model-request.ts:422` via `:427` | `generate.rs` `session_generate` | wired |
| `session.title` | `sessionID, model, `**`system, messages, options, result?`** | `session/model-request.ts:428` | `title.rs` `title_attempt` | wired |
| `session.model.request` | `sessionID, agent, model, kind, `**`baseURL?, headers`** | `session/model-request.ts:305` | `plugin_request.rs` `model_request`, after each shaping hook (primary/compaction/title/generate) | wired |
| `session.http.request` | `sessionID, agent, model, kind, `**`request`**`{url, method, headers, body}` | `session/model-request.ts:343` | `plugin_request.rs` `http_hooks` → oc-ai `HttpHook` (`Client::stream_with`), for primary/compaction/generate/title requests; the request has the prepared headers incl. `content-length`, before the trace headers | wired |
| `session.http.response` | `..., request, `**`response`**`{status, statusText, headers, body}` | `session/model-request.ts:355` | same; `statusText` is `""` as upstream re-wraps the response; the body is read in full first, only when a plugin registered this hook; the status check runs on the returned response | wired |
| `session.experimental.ws.handshake` | `..., `**`url, headers`** | `session/model-request.ts:375` | Responses websocket (not in ocd yet) | no core site |
| `session.experimental.ws.send` / `receive` | `..., `**`frame`** | `session/model-request.ts:383` / `:387` | same | no core site |
| `session.retry` | `sessionID, agent, model, error, attempt, `**`decision`** | `session/model-request.ts:399` (event built in `session/runner/retry.ts:78`) | `step.rs` after `Policy::decide_raw` (`step::Input::retry_hook`) | wired |
| `permission.evaluate` | `sessionID, agent?, action, resources, metadata?, source?, `**`effect, message?`** | `packages/core/src/permission.ts:179` | `oc-core/src/permission.rs` `evaluate_hooked` (assert/ask) via `PermissionContext::evaluate_hook` | wired (also for the "always" re-evaluation of other pending requests, with their stored metadata/source) |
| `shell.create.before` | **`command, cwd, timeout, shell, env`** | `packages/core/src/shell.ts:274` | `Plugins::shell_before`: shell tool (`CoreHost::shell_create_before`), `!` commands (`shell_cmd.rs`), `shell.create` | wired |
| `tool.execute.before` | **`tool`** (renameable), `sessionID, agent, messageID, id, `**`input`** | `packages/core/src/tool.ts:104` | `runner.rs` `CoreHooks::before`, for direct calls and calls inside Code Mode (`codemode::execute_with`) | wired (`HookError::Tool` rejects the call with that `Tool.Error`) |
| `tool.execute.after` | `tool, sessionID, agent, messageID, id, input, status` + **`result`**`{output?, content, metadata?}` or **`error`**`{message, metadata?}` | `packages/core/src/tool.ts:137` (error), `:149` (completed) | `CoreHooks::after` (after the plan agent's own hook), before content normalisation | wired |
| `aisdk.sdk` / `aisdk.language` | **`sdk`** (`model, package, options`) / **`language`** (`model, sdk, options`) | `packages/core/src/aisdk.ts:350` (`runSDK`) / `:359` (`runLanguage`) | run inside the sidecar when a model is loaded (`aisdk.load`, from oc-ai `ResolvedModel::initialize` at model resolution); never cross to Rust (JS objects) | wired |

A hook defect, such as a Promise hook throwing, is returned as `HookError::Failed`. Upstream lets
the operation fail in that case.

## Transforms: where the core must call `transform`

Upstream rebuilds a domain by applying every transform in registration order, so plugin transforms
sit between the `pre` and `post` internal plugins. At every domain rebuild, the core must:

1. Run the native `pre` transforms.
2. Call `PluginHost::transform(location, domain, value)`. This costs no IPC when no plugin
   transforms the domain.
3. Run the native `post` transforms on the result. Config wins.

Rebuild a domain when any of these happens:

- `domain_revision` changes, or `HostEvent::DomainsChanged` / `Restarted` arrives;
- a plugin calls `<domain>.reload`;
- the core's own inputs change.

### Transform values

Each value below is the JSON the core sends and gets back. Items are encoded with upstream schemas,
and functions are `{"$fn": handle}`. The upstream editors these values feed:

- `core/src/<domain>.ts`, wrapped as in `core/src/plugin/host.ts`;
- for `mcp`, `core/src/mcp/index.ts:599`;
- for `worktree`, `core/src/worktree/strategies.ts:48`.

| domain | value | functions |
|---|---|---|
| `agent` | `{agents: Agent.Info[], default?}` | — |
| `command` | `{commands: [{name, description?, execute}]}` | `execute(invocation{sessionID, prompt, delivery})` |
| `integration` | `{integrations: [{ref:{id,name}, methods: Method[], implementations: [{integrationID, method, authorize, refresh?, label?}]}]}` | `authorize(answer)` → `{url, instructions, expiresAt?, mode, callback}`; `callback` is a handle (`auto`: call with no args; `code`: call with `code`) |
| `mcp` | `{servers: [[name, ServerConfig]], removed: [name]}` | — |
| `model` | `{models: [[providerID, Model.Info[]]], default?: {providerID, modelID}, providers: ProviderRecord[]}` (`providers` is read-only input) | — |
| `provider` | `{records: [{provider: Provider.Info, models: Model.Info[], sourceConnection?}]}` | — |
| `reference` | `{sources: [[name, Reference.Source]]}` | — |
| `skill` | `{skills: Skill.Info[]}` | — |
| `tool` | `{tools: [{id, name, description, input: JSONSchema, output?, options?, execute}], namespaces: [{name, description}]}`; the answer adds `errors` | native tools: pass `execute: {"$fn": "rs:<anything>"}` |
| `vcs` | `{providers: [{id, name, init?, info, base?, branches, status, diff}], selection?}` | each op takes its upstream input object |
| `websearch` | `{providers: [{id, name, execute}], selection?}` | `execute(ProviderInput)` |
| `worktree` | `{strategies: [{id, create, remove, list}], selected?}` | `create(input)`, `remove(input)`, `list(sourceDirectory)` |

Tool entries returned with a `js:` handle are plugin tools, or native tools a plugin wrapped.
Execute them with `PluginHost::execute_tool(location, handle, input, {sessionID, agent, messageID,
id}, progress)`, which applies upstream's runtime decode, encode and normalise steps. Run
`tool.execute.before` and `tool.execute.after` around the call, as upstream `Tool.executeTool`
does. Image normalisation of the result stays in the core.

Entries returned with the core's own `rs:` handle are still native: the plugin may have edited
metadata such as the description, but did not replace the executor.

`codemode` options apply unchanged, so plugin tools reach the model directly or through Code Mode,
just like native ones.

## Core integration (done)

The core side lives in `oc-core/src/plugins.rs` (`Plugins`, owned by `App`), `oc-core/src/plugin_request.rs`
(request hooks) and `oc-backend/src/plugins.rs` (`BackendApi`). What each checklist item below maps to:

- **Host.** `Plugins` creates one `PluginHost` per process the first time a location has plugin
  operations; Bun starts only when an operation needs a package or local plugin. `Plugins::shutdown`
  runs at server exit (`ocd serve`).
- **API.** `BackendApi` serves HTTP-equivalent methods through an in-process router (same handlers),
  `integration.connection.*` over oc-ai `Integrations`; `CoreApi` serves `<domain>.reload`
  (cache dropped, `<domain>.updated` published), `tool.list`, `event.publish`, `rpc.call`
  (`rpc.unavailable`), storage (kv table) and `rs:tool.<id>` handles (the native executor with the
  in-flight call's context). `rs:command.<name>` runs native commands in the backend.
- **Loading.** `Plugins::ensure` on location load (runner and backend location caches) and on
  `Engine::reload_location`; activation is asynchronous like upstream and `await_activation`
  gates prompts, commands, RPC calls and the runner's request assembly. `HostEvent::Watch` files are
  watched (notify, 100 ms debounce, off with `OPENCODE_DISABLE_FILEWATCHER`) and reload their
  locations. `close` on location close/evict. Built-in plugins removed by configuration are gated
  for tools, `opencode.agent`/`opencode.plan`, `opencode.command`, `opencode.skill`,
  `opencode.browser` and the config plugins for agents, commands, skills, compatibility roots,
  references and MCP.
- **Inventory.** `plugin.list` = `LoadResult::inventory` with the sidecar's latest package infos;
  `plugin.updated` is published when the inventory changes after the first activation (a plugin
  disabled by a failing transform first refreshes its domains, like upstream's `State.group`).
- **Transforms** (pre → plugins → post, cached per revision and input): `tool` (registry incl. MCP
  tools; plugin tools execute in the sidecar through `execute_tool`, native tools a plugin edited
  keep their executor), `agent`, `command`, `skill`, `reference`, `mcp`, `provider` and `model`
  (`Catalog::build_staged`: after the internal provider plugins, before the configured-provider
  plugin), `integration` (plugin OAuth methods authorize through their handles in a per-attempt
  scope), `websearch` (plugin providers execute through their handles; built-ins a plugin keeps or
  re-registers run natively through `rs:websearch.<id>`), `vcs` (`Plugins::vcs` → `oc-core/src/vcs.rs`
  `Registry`: the git provider as `rs:vcs.git.<op>` handles, plugin providers and `default.set`;
  used by every `vcs.*` endpoint) and `worktree` (`Plugins::worktree_strategies` →
  `oc-core/src/worktree.rs` `Strategies`: the git strategy as `rs:worktree.git.<op>`, an added
  strategy becomes the selected one; used by `worktree.create/remove/refresh` with the project's
  canonical location, and by the worktree tools). After a location's first activation every
  transformed domain is rebuilt once, so failing transforms disable their plugin right away.
- **Events.** Every bus event is forwarded to `publish_event` while some plugin subscribed; plugin
  tool progress is published without a location (as upstream).

Integrations and web search are per location like upstream: `Models::integrations_for(key)` (one
`Integrations` registry per location over the global credential store) and
`Plugins::websearch_for(key)` (one web search service per location, forked from the built-ins); the
plugin `integration.connection.*` API uses the calling location's registry.

Known differences: `session.http.response` gets the response body read in full (upstream passes a
streaming `Response`); see docs/PARITY.md "Remaining".

### The original checklist

1. **Construct the host.** Create one `PluginHost::new(HostOptions::from_global(&global, app),
   api)` per server process. `app` is `{name: "opencode", version, channel}`.
2. **Implement `PluginApi`.**
   - Methods listed in `src/api.rs` that have an HTTP endpoint: reuse the oc-backend handlers. The
     input is the merged params, query and body.
   - The rest: `<domain>.reload`, `tool.list`, `integration.connection.*`, `event.publish`,
     `rpc.call`.
   - `storage_*`: the KV table.
   - `invoke`: native function handles.
3. **Load plugins.** On location load, config reload, or `HostEvent::Watch` for a file of that
   location, call `load(LoadRequest{location, info, operations, internal, blocked})`.
   - `operations` comes from `oc_config::sources::plugin_operations`; its JSON matches `Operation`.
   - `internal` is the ocd-native internal plugin ids in upstream order
     (`core/src/plugin/internal.ts:216-270`).
   - `blocked` is the managed-policy check `integration.use` on `plugin:<target>`.
   - Gate native internal plugins on `result.resolution.enabled_ids()`. This implements
     `-opencode.browser`, `-*`, and the guarded ids.
   - Build the `plugin.list` inventory with `result.inventory(builtin)`. Publish `plugin.updated`
     on `HostEvent::PluginsChanged`.
4. **Close locations.** When a location closes or is evicted, call `close_location`. At server
   exit, call `shutdown()`.
5. **Hooks.** Call `trigger_hook` at every site in the hook table.
6. **Domain rebuilds.** Call `transform` inside every domain rebuild, between the pre and post
   internals. Rebuild when `domain_revision`, `DomainsChanged` or `Restarted` says to.
7. **Tools and functions.**
   - Plugin tools: use `execute_tool` and route progress to the call's metadata. Map
     `ToolError{tool_error: true}` to a model-visible `Tool.Error`.
   - Commands, websearch, vcs and worktree functions, integration OAuth: use `invoke`. For an OAuth
     attempt, pass a per-attempt `scope` and `close_scope` when the attempt ends.
8. **RPC.** `POST /api/rpc/:rpcID/:method`: when `rpc_ids(location)` contains the id, call
   `rpc_call`, which returns `RpcFailure{type, message, data}` on failure. Otherwise answer
   `rpc.unavailable` / `RPC is unavailable: <id>`. For a plugin `event.publish`, publish an
   ephemeral bus event `{id, created, type, data, location}`.
9. **Events.** Forward every bus event to `publish_event(Some(location) | None, &wire_event)`. This
   is cheap: it does nothing unless `wants_events()`.

## AI SDK bridge (phase 2)

Upstream references: `packages/core/src/aisdk.ts` (the `AISDK` location node), `aisdk-native.ts`
(which packages are native), `model-resolver.ts` (`loadAISDK`), `plugin/provider/{dynamic,
sdk-factory,factory,cohere,perplexity,vercel,gitlab,sap-ai-core}.ts` and `plugin/host.ts:157`.

**Which models use it.** Exactly upstream's decision: the catalog rewrites known AI SDK packages to
native ones (`AISDKNative.rewrite`, oc-ai `catalog/native.rs`); a model whose effective package still
starts with `aisdk:` (an AI SDK package with no native implementation, e.g. `aisdk:@ai-sdk/deepseek`
under a provider id other than `deepseek`, `aisdk:@ai-sdk/perplexity`, `aisdk:file:///...`) resolves
through the bridge (oc-ai `ResolveOptions.aisdk`). Native packages never touch it.

**Split of work.**

| step | where | upstream |
|---|---|---|
| route, request lowering to `LanguageModelV3CallOptions`, raising stream parts to LLM events, error classification | oc-ai `src/aisdk.rs` | `modelFromLanguage`, `callOptions`/`prompt`, `streamLanguage`/`streamPartEvents`, `llmError` |
| package install/import, `aisdk.sdk` / `aisdk.language` hooks (internal provider plugins, then plugins), SDK and language caches, `prepareOptions` (SDK options, timed `fetch`, extra body, header/chunk timeouts), `doStream`, error shapes | sidecar `host/src/aisdk.ts` | `AISDK.Service.language`, `prepareOptions`, `wrapSSE`, `loadSDKFactory`, the internal plugins |
| JSON-RPC plumbing, part streams, `session.http.*` callbacks, npm installs | `src/aisdk.rs` (`PluginHost::aisdk_load/aisdk_stream/aisdk_close`) | — |
| per-location bridge, resolution sites | oc-core `src/aisdk.rs` (`LocationBridge`), `Models::resolve_model` | `ModelResolver.layer` |

**Lifecycle.** Resolution (`Models::resolve_model`: session steps, titles, compaction, generate)
calls `aisdk.load`, which runs the hooks and constructs the language model eagerly like upstream; a
failure is `ModelInitializationError` phase `init` ("Cannot initialize <provider>/<model>: <detail>").
Each request then sends the compiled call options with `aisdk.stream`; the sidecar forwards every
part except `stream-start`, `response-metadata`, `raw`, `file`, `source` and
`tool-approval-request` (which upstream drops) as `aisdk.part` notifications, and an `error` part or a
thrown error ends the call with its error shape. The route emits `step-start` before the request,
applies upstream's fragment rules and no terminal-event rule (like `streamLanguage`). Dropping the
stream (interrupt) cancels the call in the sidecar, which releases the provider's reader; no
`abortSignal` is passed, as upstream passes none. Caches are per location and dropped when the
location closes.

**Wire forms.** File data crosses as `{"$bytes": "<base64>"}` (`Uint8Array`) or `{"$url": "..."}`
(`URL`); base64 sources stay strings. Error shapes: `{kind: "api", message, url, statusCode, data,
responseHeaders, responseBody, isRetryable}` for an `APICallError` (detected by its
`vercel.ai.error.AI_APICallError` marker, message from `providerErrorMessage`), `{kind: "network",
message, code}` for upstream's network failure codes and messages, else `{kind: "unknown", message,
data, responseBody}` (`unknownErrorMessage`). oc-ai turns them into the same `AIError`s.

**HTTP hooks.** With `session.http.request` / `.response` plugin hooks, the SDK's `fetch` goes
through them (upstream `throughMiddleware`): the sidecar sends the request (`content-length` added
for a body) to Rust (`aisdk.http.request`), which runs the plugin hooks, fetches with the result and,
when a response hook exists, reads the body and passes it through `aisdk.http.response`. No
`b3`/`traceparent` headers (the SDK uses plain `fetch`, as upstream).

**Internal provider plugins** run first, in upstream's order, gated by the location's plugin
configuration (`-opencode.provider.dynamic` etc.):

| plugin | hooks | ocd |
|---|---|---|
| `opencode.provider.cohere`, `.perplexity`, `.vercel` | `sdk` for `@ai-sdk/cohere` / `perplexity` / `vercel` | upstream bundles these packages; ocd installs exactly upstream's pinned versions (`3.0.27`, `3.0.26`, `2.0.39`) on first use |
| `opencode.provider.gitlab` | `sdk` for `gitlab-ai-provider`, `language` for provider `gitlab` (agentic/workflow chat) | installs `gitlab-ai-provider@6.19.0`; user agent from `ctx.app` |
| `opencode.provider.sap.ai.core` | `sdk` / `language` for provider `sap-ai-core` | `loadSDKFactory` of the configured package, service-key environment |
| `opencode.provider.dynamic` | `sdk` when none set it: the package's first `create*` export | `file://` URLs are imported directly; otherwise `npm.add` (official unversioned `@ai-sdk/x` → `@ai-sdk/x@ai-v6`), then `Bun.resolveSync` + import |
| `opencode.provider.github.copilot` | `sdk` / `language` for `@ai-sdk/github-copilot` | not ported: it needs upstream's in-tree Copilot provider, and ocd's catalog never assigns that package (see Differences) |

Plugin hooks see `{model, package, options, sdk?}` / `{model, sdk, options, language?}` (the
`RuntimeInfo` model and the SDK options object upstream passes, mutations included).

**Differences from upstream.**

- Packages upstream bundles (cohere, perplexity, vercel, gitlab) are installed from npm at the same
  versions, so the first use needs the registry.
- npm install failures read `Failed to install <spec>: <bun output>` in the initialization detail
  (upstream: the npm/arborist error message).
- A missing module's message names the sidecar bundle as importer (`imported from .../host.js`;
  upstream names its compiled chunk).
- The GitHub Copilot `aisdk` hooks are not ported (above).
- Non-AI-SDK third-party packages (a package exporting upstream's `model(modelID, settings)` for the
  `@opencode/ai` runtime, loaded by `Provider.loadPackage`) are still unsupported:
  `Provider package <x> is not installed`.

## Install and discovery

- **Discovery.** `oc-config` already parses `plugins` and discovers `<dir>/plugin(s)/*`. That
  covers the remove `-` syntax, `./`, `../` and `file://` targets, and the
  `configured plugin path must be a directory` warning. The sidecar resolves entrypoints with
  upstream `Host.resolve`:
  - `server` subpath first, then the package root;
  - for a directory, `<dir>/index`;
  - a lone file from auto-discovery is its own entrypoint.
- **npm packages (`src/npm.rs`).** Packages use upstream's cache layout,
  `<cache>/npm/<key>/<generation>/node_modules/<name>`, with the same keys
  (`<name>@<spec>`, `git-<slug>-<sha12>`), staging, generations, GC policy and lock name
  (`npm-install:<dir>`). The installer is `bun add <spec> --ignore-scripts --exact`, which reads
  `.npmrc`. Bun writes `bun.lock`, not `package-lock.json`, and git revisions are read from either.
- **Differences from upstream.**
  - Missing packages are installed before activation. Upstream activates what is on disk first and
    installs in the background.
  - There is no 24-hourly update check or `outdated` / `updating` flag yet. `Npm::update` and
    `Npm::collect` exist for a future `ocd plugin update`.

## Environment variables

| variable | effect |
|---|---|
| `OCD_BUN_PATH` | Bun executable for the sidecar and `bun add` (ocd override; checked first) |
| `OPENCODE_BUN_PATH` | same, upstream's name (used when `OCD_BUN_PATH` is unset) |
| `OCD_BUN_MIRROR` | base URL replacing `https://github.com/oven-sh/bun/releases/download` |
| `OCD_PLUGIN_HOST_JS` | run this host bundle instead of the embedded one (development) |
| `OCD_TEST_BUN` | tests only: Bun used by `tests/sidecar.rs` when `OCD_BUN_PATH` is unset |

Without an override, Bun 1.4.2 is downloaded from the release for this platform:

- archives are `bun-<os>-<arch>[-musl][-baseline].zip`, and `-baseline` is picked on x64 CPUs
  without AVX2;
- each archive is checked against the SHA-256 pinned in `src/bun.rs` (from the release's
  `SHASUMS256.txt`);
- the binary is installed at `<Global.bin>/bun/1.4.2-<sha16>/bun`, with a digest stamp that is
  re-verified on use.

## Rebuilding the bundle

```sh
BUN=/path/to/bun-1.4.2 scripts/build-plugin-host.sh /path/to/opencode-checkout
```

The checkout must be at the pinned upstream commit, with `bun install` run at its root.
`host/build.ts` bundles `host/src/main.ts` with `Bun.build` into `host/dist/host.js` (about 2 MiB):

- target bun, ESM, minified;
- bare imports from `host/src` resolve from the checkout's `packages/plugin`, and
  `@upstream-core/*` resolves from `packages/core/src`;
- `runtime-modules.ts` is replaced with the static specifier map, without `@opencode/plugin/tui*`;
- the HttpApi Scalar/Swagger assets are stubbed, as in upstream's build.

`host/dist/host.js` is committed and embedded with `include_str!`, so building ocd needs neither
bun nor the checkout. After rebuilding, run the tests.

## Tests

- `cargo test -p oc-plugin`:
  - unit tests: supervisor rules, npm spec, key and layout, zip, bun platform names, protocol,
    storage namespace;
  - `tests/sidecar.rs`: spawns the real sidecar with a fake `PluginApi`.
- Fixtures in `tests/fixtures`:
  - a Promise plugin with a tool, a command, a `tool.execute.before` hook, a native-tool wrapper
    and a cleanup;
  - an Effect plugin with agent and skill transforms, a failing `Tool.Error` hook, and
    provider-scoped `session.context` and `session.retry` hooks;
  - storage, event subscription and an RPC with an event;
  - an invalid module;
  - a throwing transform;
  - upstream's internal `opencode.mcp.codemode.defaults` copied verbatim (public API only);
  - `permission.evaluate`, `session.prompt`, `session.context`, `shell.create.before` and
    `tool.execute.after` hooks (`permission-plugin`);
  - provider, model, integration and websearch transforms plus a Code Mode tool (`catalog-plugin`);
  - `session.http.request` / `session.http.response` hooks rewriting model requests and responses
    (`http-plugin`);
  - a selected `vcs` provider and a `worktree` strategy (`vcs-plugin`);
  - `aisdk-provider`: a dependency-free AI SDK provider package (LanguageModelV3, factory
    `createFixture`) with a scripted mode and a mode that talks OpenAI chat-completions SSE to a
    `baseURL` through the SDK `fetch` (V3 prompt as `messages`), with `APICallError` markers;
  - `aisdk-plugin`: provider-scoped `aisdk.sdk` (wraps the SDK, renames the API model) and
    `aisdk.language` (wraps the language model, adds a header) hooks.
- `tests/aisdk.rs`: the bridge against the real sidecar (scripted stream through the dynamic
  plugin without any plugin loaded, error shapes, initialization failures and built-in gating,
  plugin hooks with the HTTP middleware against a local SSE server, cancellation).
- The core integration is tested in `oc-backend/tests/plugins.rs` and `oc-backend/tests/aisdk.rs`
  (text, reasoning, tool calls and usage through the session runner, plugin `aisdk.*` and
  `session.http.*` hooks, provider errors, a package that fails to load) with these fixtures, and
  in the parity suite (`scripts/parity/plugins.py`, group `plugin`, incl. `plugin_aisdk` and
  `plugin_aisdk_hooks`) against upstream.
- `cargo test -p oc-plugin --test sidecar -- --ignored` also installs the published v2 plugin
  `opencode-models-discovery@1.10.0` from npm with `bun add` and activates it. This needs network.
- Bun is taken from `OCD_BUN_PATH` or `OCD_TEST_BUN`. When neither is set, the tests fall back to
  the session scratchpad bun. When no bun is found they are skipped, with a message.

# oc-tools

The built-in tools of the opencode v2 Rust port (`packages/core/src/tool/**` upstream), plus
the registry that builds a request's tool list. Spec: `docs/spec/04-tools.md`.

Core (`oc-core`) owns sessions, permissions, jobs, MCP and config. It reaches the tools through
three things: the `Tool` trait, a per-call `ToolContext`, and the `ToolHost` trait, which core
implements so tools can call back into it.

## Tool interface (`src/tool.rs`)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;                                   // registered (un-namespaced) name
    fn options(&self) -> ToolOptions { ToolOptions::direct() }
    fn definition(&self, ctx: &DefinitionContext) -> ToolDefinition;
    fn advertised(&self, ctx: &DefinitionContext) -> bool { true }   // session-hook removal
    fn validate(&self, input: &Value) -> Vec<validate::Issue>;       // default: JSON schema, Effect messages
    fn prepare_input(&self, input: Value) -> Value { input }         // plugin `tool.execute.before` rewrites
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError>;
}
```

- **`ToolOptions`** `{namespace, permission, codemode, pinned}` mirrors `Tool.Options`.
  - `codemode: false` puts a tool on the provider's direct list; `true` routes it through
    CodeMode's `execute`.
  - The effective model name is `effective_name(name, namespace)`, e.g. `opencode_models`.
- **`ToolDefinition`** serializes as `{type:"tool", name, description, inputSchema, outputSchema?}`.
  - Built-in input and output schemas are the JSON that upstream's `definition()` emits,
    captured by running the real code. They are stored byte-exact in `schemas/*.json`.
  - Output schemas are only consumed by CodeMode signatures; providers never see them.
- **`DefinitionContext`** carries the inputs of the session `context`/`compaction`/`generate`
  hooks. `DefinitionContext::default()` gives the registered (pre-hook) definitions. Its fields:
  - `model_id`: drives the `gpt-` patch versus edit/write swap.
  - `shell`: the compatible shell path, used in the shell description.
  - `subagents`: already filtered; the tool sorts them.
  - `websearch_disabled`: removes `websearch` from the list.
- **`ToolResult`** `{output: Option<Value>, content: Vec<oc_schema::ToolContent>, metadata: Option<Map>}`
  mirrors `session.tool.success` plus the declared output.
  - Empty `content` is normalized to `[text: JSON.stringify(output)]`.
- **`ToolError`** `{message, cause: Option<ErrorCause>, content, metadata, kind}`.
  - `ErrorCause` is one of `Error(msg)`, `Tool(Box<ToolError>)` or `PermissionRejected(msg)`.
  - `to_session_error()` reproduces `toSessionError`: the cause is unwrapped recursively and an
    empty cause message falls back to the leaf message. `model_message()` is the text the model sees.
  - `kind` is one of `Failure`, `Declined`, `QuestionDismissed`, `LocationClosed` or `Interrupted`.
    Every kind except `Failure` is a defect: it tunnels past leaf error mapping, and core must end
    the step with `{type:"aborted"}` as described in spec 03 §2.11.
- **`ToolContext`** (cloneable) carries:
  - ids: `session_id`, `message_id`, `call_id`, `agent`
  - location: `directory`, `worktree` (project dir), `project_id`, `workspace_id`
  - `global: Arc<oc_util::Global>` and `cancel: CancellationToken`
  - `progress` callback: publishes `session.tool.progress`
  - `config: Arc<ToolConfig>`: `tool_output.max_*`, resolved `formatter` config,
    `experimental.portable_shell_scanner`, `experimental.subagent_depth`, `shell`, app `channel`
    and `media.image` limits
  - `host: Arc<dyn ToolHost>`

## ToolHost (`src/host.rs`)

Each method has a default that reports the capability as unavailable. The exceptions are
`ask_permission`, which has no default, and `job_start`, whose default spawns the job inline.

| area | methods |
|---|---|
| permissions / forms | `ask_permission(PermissionRequest) -> Result<(), PermissionError>`, `ask_form(FormRequest) -> FormResponse` |
| read tool | `load_instructions(session, paths)`: nested AGENTS.md |
| skills | `get_skill(id)`, `list_skills()` |
| sessions | `session_get/create/switch_agent/switch_model/prompt/interrupt/rename/move/synthetic`, `session_environment` |
| agents / models | `agent_resolve`, `models_available`, `models_list`, `providers_list` |
| jobs | `job_start(JobStart{run: BoxFuture})`, `job_block`, `job_background`, `job_wait`, `job_cancel`, `job_complete_background`, `subagent_job_start/background/notify` |
| shell | `shell_create_before(&mut ShellInvocation)`: plugin hook |
| worktrees | `worktree_create/list/remove` (remove returns `WorktreeRemoveError` classes) |
| MCP | `mcp_servers`, `mcp_resource_catalog`, `mcp_resources`, `mcp_read_resource`, `mcp_call_tool` |
| formatter | `npm_which(pkg)`: used by prettier, oxfmt and biome detection |
| notifications | `files_changed(session, paths)`: no-op hook for watcher/LSP |

`PermissionRequest` carries `{session_id, agent, action, resources, save: Option, metadata, source{messageID,id}}`.
`PermissionError` is one of:
- `Declined`: a defect
- `Corrected{feedback}`: typed; the model sees the feedback
- `Blocked{message}`
- `LocationClosed`
- `Other`

## Registry (`src/registry.rs`)

- `Registry::add` validates registrations exactly as upstream: namespace segments, the
  normalized name, and `execute` being reserved. The last valid registration for an effective
  name wins and keeps the position of the first.
- `Registry::namespace` registers namespace descriptions.
- `tools::register_builtins(&mut registry, &Services)` registers the built-ins in upstream
  order. `Services` holds `{formatter, ripgrep, shell, websearch, http}`.
- `registry.snapshot(&rules) -> Snapshot`:
  - Tools are hidden when the last rule matching `options.permission ?? name` is
    `resource:"*", effect:deny`.
  - `direct` tools are sorted by UTF-16 code units, and `codemode` tools stay in registration
    order for the catalog.
  - `execute_enabled` is false when `execute` itself is wholly denied.
- `snapshot.definitions(&DefinitionContext, Some(execute_def))` returns the request definitions.
  - It applies `advertised`/description hooks to the direct tools, then appends the CodeMode
    `execute` definition (built in oc-codemode/core) last.
- `snapshot.execute(name, input, ctx, Some(&advertised_names), &hooks)` runs a direct call:
  1. `prepare_input`, then the `ExecuteHooks::before` hook.
  2. Fails with `Tool is not available for this request: …` or
     `No tool named "…" is currently available. …` when appropriate.
  3. Input validation; errors use upstream's `Invalid arguments for tool …` format with
     Effect issue texts.
  4. Execution races `ctx.cancel`.
  5. `after` hook, content normalization, then image normalization (`[N image(s) omitted: …]`).
- `registry::execute_tool(tool, name, input, ctx, hooks)` is the same pipeline for one tool;
  CodeMode uses it for nested calls.
- After execution, core applies `tool_output::truncate(result, max_lines, max_bytes, &global.data)`
  as upstream's step runner does. Core also runs `tool_output::spawn_cleanup(data)` and
  `ShellService::cleanup(data)` hourly.

## Modules

| module | upstream |
|---|---|
| `tools::{read, edit, write, patch, grep, glob, shell, webfetch, websearch, question, skill, subagent}` | `core/src/tool/plugin/*.ts` |
| `tools::opencode` (6 tools + namespace + `SYSTEM_HINT`), `tools::mcp_resource`, `tools::mcp::McpTool` | `opencode.ts`, `mcp-resource.ts`, `tool/mcp.ts` |
| `file_access`, `file_mutation`, `file_diff`, `jsdiff` (byte-exact jsdiff 8.0.4 port) | `file-access.ts`, `file-mutation.ts`, `file-diff.ts` |
| `patch` (grammar, 4-pass seek), `validate` (Effect-compatible issues) | `util/src/patch.ts`, `tool/runtime.ts` |
| `ripgrep` (args, JSON parsing, binary locate/download of 15.1.0 into `<cache>/bin`) | `ripgrep.ts`, `ripgrep/binary.ts` |
| `shell::{select, parse, scan, service, result}`: tree-sitter (legacy) and portable scanners | `shell/*.ts`, `shell.ts` |
| `html` (htmlparser2 8.0.2 + `html-markdown.ts` transliteration) | `tool/html-markdown.ts`, `webfetch.ts` |
| `websearch` (service, 5 providers, Console provider) | `websearch.ts`, `plugin/websearch/*` |
| `formatter` (26 built-ins, overrides), `image` (resize ladder), `mime`, `tool_output` | `formatter*.ts`, `image*.ts`, `mime.ts`, `tool-output.ts` |

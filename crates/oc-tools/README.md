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
  - `global: Arc<oc_util::Global>` and `cancel: CancellationToken` (interrupts the call)
  - `stop: Stop`: the user's cooperative stop (`Tool.Context.cancel`, 2.3.0). A tool that listens
    (`stop.listen()`, held for as long as it would stop on its own) is asked to settle its own call when the
    user stops the step; the step interrupts it through `cancel` only if it is not listening or does not
    settle within 5 s. `Stop::none()` never fires (nested Code Mode calls, calls outside a step). The shell
    tool listens for the whole call: a stop while it asks for permission fails with `Command interrupted by
    the user before it started.`; a stop while the command runs stops it (`ShellService::stop`: SIGTERM, then
    SIGKILL after 3 s, the command keeps its status `killed` and its file), waits up to 500 ms for it to exit,
    and fails the call with `ShellResult::interrupted` (the output so far, `[full output saved to F]` or
    `[no output; output file: F]`, then `Command interrupted by the user (shell ID: X)[; it is still being
    stopped].`) and metadata `{interrupted, truncated, shellID, notice}`. Cancelling a shell job (or
    interrupting its call) stops the command and keeps it instead of removing it.
  - `progress` callback: publishes `session.tool.progress`
  - `config: Arc<ToolConfig>`: `tool_output.max_*`, resolved `formatter` config,
    `experimental.subagent_depth`, `shell`, `media.image` limits and `image_model`
    (`experimental.image_generate.model`)
  - `host: Arc<dyn ToolHost>`

## ToolHost (`src/host.rs`)

Each method has a default that reports the capability as unavailable. The exceptions are
`ask_permission`, which has no default, and `job_start`, whose default spawns the job inline.

| area | methods |
|---|---|
| permissions / forms | `ask_permission(PermissionRequest) -> Result<(), PermissionError>`, `check_permission(PermissionRequest)` (`assertNotDenied`: fails only on a deny, never asks; webfetch checks each redirect target with it), `permission_allows(PermissionRequest) -> bool` (`allows`: true only when already allowed, never asks; `edit` shows its miss hint only then), `ask_form(FormRequest) -> FormResponse` |
| read tool | `load_instructions(session, paths)`: nested AGENTS.md (or CLAUDE.md where a directory has none, while `ToolConfig.instructions_claude`) |
| skills | `get_skill(id)`, `list_skills()`; `SkillInfo.files` holds the virtual files of a `skill://<id>/SKILL.md` skill |
| sessions | `session_get/create/switch_agent/switch_model/prompt/interrupt/rename/move/synthetic`, `session_environment`; `SessionSummary.directory` is the session's location, which `session_move` resolves `~` and relative paths against |
| agents / models | `agent_resolve`, `models_available`, `models_list(types)` (the `opencode_models` tool's `type` filter, chat by default), `providers_list`, `generate_image(request)` (`Generate.image` at the session's location; `ModelSummary.input` tells which image models take input images) |
| jobs | `job_start(JobStart{run: BoxFuture})`, `job_block`, `job_background`, `job_wait`, `job_cancel`, `job_complete_background`, `subagent_job_start`, `subagent_running` (2.3.0: subagents always run in the background) |
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
     Effect issue texts (Effect schemas count string lengths in UTF-16 units: `Expected a value with a
     length of at most N`; raw JSON Schema inputs count code points).
  4. Execution races `ctx.cancel`.
  5. `after` hook, content normalization, then image normalization (`[N image(s) omitted: …]`).
- `registry::execute_tool(tool, name, input, ctx, hooks)` is the same pipeline for one tool;
  CodeMode uses it for nested calls.
- `snapshot.repair_input(name, &mut input)` is the built-in `opencode.tool.input.repair` plugin
  (`repair::repair_input`, `plugin/tool-input-repair.ts`). Core runs it first in its `before` hook, for
  direct and nested CodeMode calls, unless the location removed the plugin. It looks the tool up among
  all registered tools (`snapshot.registered`, permissions aside), skips `execute`, and repairs the input
  against the registered input schema when that is a `type: "object"` schema:
  - stringified objects and arrays are parsed;
  - closed objects lose undeclared keys;
  - optional plain-typed nulls and `{}` placeholders are dropped;
  - numeric strings are converted with JS `Number()`, and `"true"`/`"false"` become booleans;
  - single non-null union alternatives, tuples, typed dictionaries and local `$ref`s are followed;
  - a scalar is wrapped into a one-item array when it fits the item type;
  - nesting stops at depth 6.

  Validation then runs on the repaired input, so the "Arguments provided" text of an
  `Invalid arguments` error shows it. The stored tool input stays as the model sent it.
- `snapshot.alias(name)` is the same plugin's alias table (`bash` and `Bash` -> `shell`), used only when no
  registered tool has the called name. Core renames the call in its `before` hook and, in `after`, puts a
  `<tool-repair>` note first (2.4.1): `There is no "bash" tool, so this ran as "shell". Call "shell" next time.`
  before the failure message or as the first text part. A top-level call to a Code Mode tool (its path or
  registry ID) runs as a Code Mode call when the request offered `execute`, noted with
  `<name> is a Code Mode tool, so this ran inside execute as tools.<path>(input). Call it there next time.`
- After execution, core applies `tool_output::truncate(result, max_lines, max_bytes, &global.data)`
  as upstream's step runner does (2.3.0: one cut at the line or byte limit, `(last line cut)`, grapheme-safe; when
  the full output can't be saved the preview stays and the marker gives Node's error text). Core also runs `tool_output::spawn_cleanup(data)` and
  `ShellService::cleanup(data)` hourly.

## Modules

| module | upstream |
|---|---|
| `tools::{read, edit, write, patch, grep, glob, image_generate, shell, webfetch, websearch, question, skill, subagent, todo}` | `core/src/tool/plugin/*.ts` (`todo`: `todowrite`, which oc-core keeps only where `experimental.todo` is on; `image_generate`: `generate_image` (2.4.1; `experimental_image_generate` before, and permission rules naming the old names are rewritten), which oc-core keeps only while an image model is available and `experimental.image_generate` is not `false`; it checks permissions and reads input images up front, then generates in a background job whose `<background-result kind="image">` notice gives the saved path). `webfetch` uses its own client that never follows redirects: it follows up to 10 itself, checking each target with `check_permission`; the rules see URLs without their `#fragment`, and URLs with credentials are refused (`URL must not include credentials`). `read` (2.2.7): a negative `offset` counts back from the last line (directories refuse it), an offset past the end returns the last lines (at most 100, under the 50 KB cap) with a `(offset N is past the end; the file has M lines)` header, a missing file suggests close names (same name in another case, same stem, containment, then typos, plus the path re-rooted under the location when reading it is already allowed), and `skill://<id>[/<file>]` reads a skill's in-memory files after the `skill` and `read` approvals. `skill` shows `skill://<id>` as the base directory of an in-memory skill. `shell` (2.3.1) moves a foreground command that outlives its timeout (default 2 min; 0 means the default) to the background with `did not finish within its timeout of N seconds`, while an explicit timeout still stops a background command; it ends results with `Exit code: N` (`143 (SIGTERM)` for a signal, from the platform's signal numbers), says how long a timeout waited, records how a timed-out process exited, and a truncated tail that starts mid-line drops the partial line or names how much of a single long line is shown. `opencode.session_move` asks for `external_directory` before a move out of the project that none of its worktrees contains. `patch` chains later hunks onto files an earlier hunk added, moved or deleted, a move onto the file itself is a plain update, and (2.3.0) a move that only changes the case renames in place. 2.3.0 result shapes: `edit`/`write` return their summary string as the structured output, `skill` `{directory, content}`, `patch` `{}` (summary from the file diffs), `shell` (2.4.1 order, small fields first) `{shellID?, exitCode, signal?, killed?, outputPath?, pid?, wallTimeSeconds, output}` with a `PID:` line and `pid` metadata when backgrounded; it passes its tool call and whether it waits to `ShellService::create`, and while a restart handoff is prepared an interrupted call leaves its command and job running (`job`, `background_tool_result` and `job_notice` are shared with oc-core's restart recovery). 2.3.1: `read` returns `{type:"text", content, offset, lineCount, next?}`, `{type:"directory", entries, next?}` or `{type:"media", uri, mime, name}` (media bytes only in the model's file part), pages small files over 2,000 lines or 50 KB, refuses `limit: 0`, and detects media by signature only (`mime::detect_media`); `websearch` returns `[{url, title?, content?, published?}]` with the provider in its metadata. `subagent` names image and decision models in its "needs a chat model" error, and (2.3.0) always runs the child in the background: no `background` input, status `running` only, and the child's reports reach the parent as `<subagent>` notices each time it goes idle (oc-core). |
| `tools::code_mode` (`full_codemode`, 2.3.1, opt-in): `variant` turns a registered `shell`, `read`, `edit`, `write`, `patch` or `websearch` into its Code Mode registration (`codemode: true`, the first five `pinned`); `shell`'s has no `background` input, a timeout defaulting to `CODE_MODE_TIMEOUT_MS` (10 min, filled in before the call when absent or 0) and no background sentence in its description, `read`'s description (`READ_DESCRIPTION`) says images and PDFs attach to the `execute` result. `HIDDEN` (`grep`, `glob`, `webfetch`) are the tools oc-core drops while the flag is on | the `fullCodeMode` branches of `shell.ts`, `read.ts`, `edit.ts`, `write.ts`, `patch.ts`, `websearch.ts`, `grep.ts`, `glob.ts`, `webfetch.ts` |
| `tools::opencode` (6 tools + namespace + `SYSTEM_HINT`), `tools::mcp_resource`, `tools::mcp::McpTool` | `opencode.ts`, `mcp-resource.ts`, `tool/mcp.ts` |
| `file_access`, `file_mutation`, `file_diff`, `jsdiff` (byte-exact jsdiff 8.0.4 port: `createTwoFilesPatch`, `structuredPatch`, `parsePatch`, `reversePatch`, `applyPatch`) | `file-access.ts`, `file-mutation.ts`, `file-diff.ts` |
| `patch` (grammar, 4-pass seek), `validate` (Effect-compatible issues) | `util/src/patch.ts`, `tool/runtime.ts` |
| `repair` (schema-driven input repair, JS `Number()`) | `plugin/tool-input-repair.ts` |
| `ripgrep` (args, JSON parsing with lines cut at 2000 UTF-16 units on a whole character and non-UTF-8 `bytes` decoded lossily (2.3.1), `InvalidGlob` for a filename glob rg cannot parse (`Invalid glob in pattern/include …`), binary locate/download of 15.1.0 into `<cache>/bin`, verified against the pinned per-platform SHA-256 before it is written) | `ripgrep.ts`, `ripgrep/binary.ts` |
| `shell::{select, parse, scan, service, result}`: the portable permission scanner, always on since 2.2.5 (`experimental.portable_shell_scanner` is accepted and ignored). `scan` covers redirects (commandless and redirected commands are resources saved as `{prefix} *`), Zsh `&!`/`&\|` and glob qualifiers, eval sinks, and PowerShell member and method calls (`$x.Kill()` saves `$x.Kill(*`); `parse` keeps dynamic or incomplete `cd`/`Set-Location` (`cd $X`, `cd -`, `popd`, `-StackName`, dot-wildcards) as resources and resolves `~+`/`~user`. Tests replay a stratified sample of upstream's recorded calls (`scan/golden.rs`); `OCD_SHELL_ORACLE=<jsonl>` replays a full recording. Unknown ANSI-C escapes in `$'…'` decode per dialect like 2.3.1 (Bash keeps the backslash, Zsh drops it, `unsupported-escape` where the shells disagree), and `service::output` pages never split a UTF-8 character. `shell::daemon` (2.3.1 `daemonLayer`): once a server calls `ShellService::attach_daemon(PersistentPty)`, `ShellService::create(invocation, metadata, Placement {project_id, directory, workspace_id, tool_call, waited})` runs a local command as a pipe process of the `opencode-pty` daemon (cwd made absolute, the invocation's environment only, metadata `Stored {id, command, shell, location, metadata, toolCall?, timeoutDeadline?, stopReason?}`), falling back to `spawn` only when the daemon is unavailable (a rejected request fails instead). Commands are grouped by location (`Place`); the first `recover`/`create` for a location in a server takes its commands back (once, on its own task). Stop and timeout signal the daemon (SIGTERM, SIGKILL 3 s later, recorded in the metadata); `wait` runs on its own connection; a lost daemon ends a command `killed` with `LOST`; exited records beyond 25 are terminated in the daemon before their file is deleted; `cleanup_all` keeps the files of commands the daemon still runs and forgets finished ones whose file is gone or that ended over 7 days ago. Results read the daemon's file: its line count when reported (else counted up to 16 MiB), line starts read back up to 1 MiB, `[output file F stopped early: …]` after a write error. `by_tool_call`, `location_of` and `handoff_prepared` serve restart recovery and the API. Tests: `shell/daemon_tests.rs` (ported `shell-daemon.test.ts`, against `oc_persistent_pty::testing::FakeDaemon`) `select::args` runs zsh with `+o nomatch`, so an unmatched glob passes through as in bash; `util::which` and `select` accept Microsoft Store app execution aliases (WindowsApps reparse points that `stat` refuses), found through `FindFirstFileW`'s reparse tag | `shell/*.ts`, `shell.ts`, `util/which.ts` |
| `html` (htmlparser2 8.0.2 + `html-markdown.ts` transliteration) | `tool/html-markdown.ts`, `webfetch.ts` |
| `websearch` (service, 5 providers, Console provider, which follows at most 5 redirects itself and drops the credential headers when one leaves the Console's origin (`oc_util::http::follow`); `set_deny` withholds providers at read time, for the policy plugin's `integration.use` transform) | `websearch.ts`, `plugin/websearch/*` |
| `formatter` (26 built-ins, overrides), `image` (`photon-normalize.ts` rules: fitting PNGs over 384 KiB and 256×256 px that are opaque become JPEG q80 when smaller; the resize ladder tries PNG first only for transparent or small sizes, then JPEG 80/70/55/40), `mime`, `tool_output` | `formatter*.ts`, `image*.ts`, `mime.ts`, `tool-output.ts` |

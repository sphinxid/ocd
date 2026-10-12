# oc-config

The location configuration system of opencode v2, ported from TypeScript to Rust. It covers
discovery, JSONC with `{env:}`/`{file:}` substitution, v1→v2 normalization, folding values per
key, Markdown agents/commands/skills, managed policy, and the `cli.json` client configuration.
The behavior is specified in `docs/spec/01-cli-config-service.md` §6.

The crate contains only pure logic and file IO. `oc-core` owns everything stateful: watchers,
the reload loop, the event bus, wellknown HTTP and credentials, shell execution, and the
agent/provider registries. It uses the functions below to do that work.

## Modules and public API

### `schema`: the typed v2 schema (`Config.Info`)
- `Info` and every sub-type: `ModelSelection`, `Agent`, `Request`, `Formatter`/`FormatterEntry`,
  `Lsp`/`LspEntry`/`LspServer`, `Media`/`MediaImage`, `ToolOutput`, `Mcp`/`McpTimeout`/`McpServer`
  (`McpLocal`, `McpRemote`, `McpOAuthSetting`, `McpProtocol`), `Compaction`, `Command`,
  `Reference`, `Plugin`, `Worktree`, `Warming`/`WarmingInfo`/`Duration`, `WebSearch`,
  `Provider`/`ProviderModel`/`Settings`/`ModelSettings`/`Compatibility`/`Capabilities`/`Variant`/
  `Cost`/`CostSetting`/`Limit`, `Experimental`/`Policy`, `UpdatePolicy`, `Share`, `Patch`.
- `Entry::{Document { path?, info }, Directory { path }}`. Serialization matches the HTTP API
  (`{"type":"document",...}`).
- Decoding goes through `decode::Decode`, which reproduces Effect Schema:
  - excess keys are ignored;
  - optional keys may be absent but may not be `null`;
  - `PositiveInt`, `NonNegativeInt`, `Int` and `Finite` are checked;
  - patterns are checked for colors, model selections and the trimmed `worktree.directory`.
- `serde::Deserialize` is implemented through `Decode`.
- Serialization is the canonical encoding: fields come out in schema order and absent fields
  are omitted. `Info::from_value` and `Info::to_value` are shorthands.
- `Duration` reproduces Effect's `DurationFromString`. `"4 minutes"` encodes as
  `"240000 millis"`.

### `normalize`: v1 → v2 migration
- `normalize(&Value) -> NormalizeResult::{Normalized { encoded, diagnostics }, Rejected { diagnostics }}`.
- `Diagnostic { kind: Conflict|Invalid|Unsupported, path, message }`, with `log_path()` (`$` or
  `$.a.b`). Messages are verbatim, available as the constants `MSG_*`.
- It implements every rule of `normalize.ts`:
  - `prefer`, `mergeMaps` and `overlay` conflicts;
  - providerID renames and `normalizeAction`;
  - `small_model` → `agents.title`;
  - precedence `agent < mode < agents`;
  - the v1 permission, tools, agent, command, provider, model and MCP mappings;
  - enabled and disabled providers → policies;
  - the lists of unsupported keys;
  - per-entry and per-item recovery.
- `legacy` holds the v1 decoders and migrations: `decode_agent`, `migrate_agent`,
  `decode_command`, `decode_provider`, `migrate_provider`, `decode_mcp`, `migrate_mcp`,
  `decode_permission`, `model_selection`, `provider_id`, `normalize_action` and `aisdk`.

### `discovery`: sources and the watch plan
- `Options { project, global, file, content }` and `Options::from_env()`, which reads
  `OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_CONFIG_PROJECT_DISABLE` and
  `OPENCODE_DISABLE_PROJECT_CONFIG`. `filewatcher_disabled()` reports the watcher switch.
- `discover(global_config, home, directory, &Options) -> Sources { global, explicit, direct,
  project: [ProjectRoot{path,present}], claude, agents }`. It walks upward to `/`, resolves every
  candidate with realpath, and filters out global roots.
- `Sources::compatibility()` returns the existing `.claude` and `.agents` roots.
- `plan(&Sources)` / `Sources::watch_plan()` returns `Vec<WatchTarget>`, which is either
  `Directory { path, ignore }` (recursive) or `Entries { path, names }` (parent watch).
  `WatchTarget::key()` gives the JSON reconciliation key.
- Debounce rules for core:
  - every watcher update is republished to domain plugins and requests a reload;
  - reload requests coalesce (sliding, size 1) and are debounced for `DEBOUNCE_MS` (100 ms);
  - reloads are serialized;
  - `config.updated` is published only when `load::changed(prev, next)` is true;
  - domain plugins debounce their own rebuilds for 100 ms;
  - wellknown sources are refreshed every `WELLKNOWN_REFRESH_MS` (10 minutes).

### `load`: the ordered entry list
- `Loader { global_config, home, directory, options, wellknown }`, built with
  `Loader::new(&Global, dir, opts)` and `.with_wellknown(Arc<dyn WellKnown>)`.
  - `discover()`
  - `load(&Sources).await`
  - `load_local(&Sources)`
  - `entries().await`, which returns `(Sources, Vec<Entry>)`
- The load order is: wellknown, `G/opencode.json`, `G/opencode.jsonc`, `Directory(G)`, the
  explicit file, the direct project files (farthest first), each present `.opencode`
  (`json`, `jsonc`, `Directory`), and finally `OPENCODE_CONFIG_CONTENT`.
- `parse_info(text, source)` logs `tracing::warn!("configuration normalization diagnostic",
  source, path, kind, action)` with the verbatim actions. Other helpers: `load_file`,
  `load_directory`, `load_content` and `read_safe` (NotFound and PermissionDenied count as
  absent).
- The `WellKnown` trait is implemented by core. It provides `entries`, `variables`, `resolve` and
  `cached`, and the loader logs `failed to discover wellknown config` and
  `failed to load wellknown config`.
- `LoadError::{Invalid(ConfigInvalidError), Io}` is fatal to a location load.
  `to_named_error()` builds the error object and `format_invalid()` produces the TUI text.
- `update_shell(dir, Option<&str>)` implements `PATCH /api/experimental/config`. It edits the
  JSONC text while preserving comments.

### `fold`: per-key merge semantics (spec table §6.6)
- Single values:
  - `latest`, `shell`, `model`, `default_agent`, `snapshots` and `tool_output`
    (defaults `TOOL_OUTPUT_MAX_*`);
  - `websearch`;
  - `warming` (defaults, the validity check and the verbatim warning);
  - `experimental`, `subagent_depth` and `portable_shell_scanner`.
- Field-wise or accumulated values:
  - `compaction` and `media_image` (overlays);
  - `worktree_directory`;
  - `watcher_ignore` (concatenated);
  - `formatter` (boolean reset and per-name merge);
  - `mcp_timeout` and `mcp_servers` (per-server timeout merge).
- Agents:
  - `permissions` (concatenated and `~`/`$HOME` expanded), `expand_home` and
    `expand_permissions`;
  - `AgentState`, plus `agents(docs, home, existing) -> AgentFold`, which handles overlay,
    appended permissions, `disabled` removal and `default_agent`.
- `references` returns `ReferenceSource`, with alias validation and path resolution.
- Providers:
  - `configured_providers`, `merge_overlay`, `merge_headers` (case-insensitive),
    `merge_capabilities`, `normalize_cost` and `overlay_model`;
  - `providers(entries)` composes these into one overlay per provider id. It does not inherit
    from the catalog.
- `plugins` and `skills` (deduplicated union).

### `sources`: directory-based configuration
- Agents:
  - `decode_agent_file` reads `{agent,agents}/**/*.md` and `{mode,modes}/*.md`, detects v1
    frontmatter and migrates it, and joins `model#variant`;
  - `markdown_agents(dir)` and `agent_documents(entries)`.
- Commands: `decode_command_file`, `markdown_commands` and `commands(entries)`. The latter lets a
  later definition replace an earlier one and puts Markdown commands in their Directory's slot.
- Skills:
  - `parse_skill` (id rules and autoinvoke metadata), `scan_skill_files` (`{*.md,**/SKILL.md}`)
    and `scan_skills`;
  - `skill_sources(entries, home, location)` returns `SkillSource::{Directory, Url}`;
  - `compatibility_skills(claude, agents)`.
- Plugins: `discover_plugin_directory`, `parse_plugin`, and `plugin_operations(entries,
  location)`, which returns `PluginOperation::{Add, Remove}`.
- Change filters: `is_agent_source`, `is_command_source` and `is_plugin_source`.
- `SUBAGENT_PREFIX`.

### `template`: evaluating command templates
- `expand(template, input)` handles `$1..$N` (the last placeholder takes the rest),
  `$ARGUMENTS`, appending the input when no placeholder is present, and the JavaScript quirks of
  `$0`.
- `parse_arguments` and `shell_commands`.
- `evaluate(template, input, run).await` runs `` !`cmd` `` with a concurrency of 2 and keeps the
  output in order. Failures produce the verbatim
  `Shell interpolation failed for "<src>": <msg>`.

### `markdown`: gray-matter-compatible frontmatter
- `matter`, `parse` (retries with `sanitize`), `parse_option` and `sanitize`.

### `policy`: managed policy and update policy
- `wildcard_match`, `ManagedState`, `statements(entries, &org)` (documents reversed, then the
  organization's statements, with verbatim messages), `decision`, `find`, `provider_allowed`,
  `mcp_allowed`, `skill_allowed` and `tool_denial`.
- `decode_update_policy(text)` and `read_update_policy(global_config)` read `config.json`,
  `opencode.json` and `opencode.jsonc`, defaulting to `notify`.

### `redact`: `opencode debug config`
- `SECRET_NAME` (the exact regex), `redact_config(&Value)`, and `render_debug_config`, which
  produces 2-space JSON plus a newline.

### `cli`: the `cli.json` client configuration
- `CliConfig::new(config_dir, state_dir)` reads `OPENCODE_CLI_CONFIG_CONTENT` once;
  `CliConfig::with_content` takes the content as an argument.
- `get().await` takes the file lock, migrates, then merges the file with the inline content.
- `update(|v| ...).await` diffs the result into leaf JSONC edits that keep comments, then
  writes `cli.json.tmp` (mode 0600) and renames it. It returns `merge(file, inline)`. Errors are
  `"Failed to update CLI config"`, with the cause `"Invalid CLI config update"` where it applies.
- `config::{merge, merge_records, changes, decode_content, path}`.
- `schema`:
  - `SCHEMA_URL` and `decode`, a whole-file decode in which unknown keys are stripped;
  - `normalize_legacy_tabs`;
  - `resolve(info, terminal_suspend, herdr_env)` returns the resolved defaults without the
    keybind lookup.
- `migrate::{run, migrate_text, migrate_v1, parse_jsonc}`:
  - in-place `copy_on_select` migration;
  - keeping only the last `keybinds` object, plus keybind renames and removals with
    comma/comment handling;
  - `tui.json` + `kv.json` → `cli.json`.
- `keybind`: the verbatim `DEFINITIONS` table (241 entries), `decode_overrides`, `parse`
  (`Unrecognized keybind(s): …`) and `default_value`.
- `v1`: `COMMAND_MAP` (162 entries) and `decode_tui_v1`.

### `jsonc_edit`: a port of jsonc-parser 3.3.1
- Scanner, `parse`, `parse_tree`, `find_node_at_location`, `format`, `modify`, `apply_edits`
  and `apply_modify`. Offsets are in bytes; the resulting text is identical to upstream.

## Not in this crate (core's job)
- Watchers and the reload loop. This crate supplies the plan, the debounce constants and
  `changed()`.
- Bus events and credential-driven reloads.
- Wellknown HTTP and caching. This crate supplies the `WellKnown` trait.
- Provider catalog inheritance (`canonical`) and model registry semantics. `fold::providers` is
  only the overlay composite.
- Plugin entrypoint and mtime resolution: `Host.resolve`, and `fs.stat` on the entrypoints.
- Running shell commands for templates. The caller passes a `run` closure.
- The TUI keybind lookup structure (opentui).
- Generating the JSON schema for `cli.json`.

## Known deviations
- YAML frontmatter is parsed with `serde_yaml`, so timestamps stay strings where js-yaml would
  produce `Date` objects. A language tag other than `yaml` errors.
- Object keys keep their insertion order; JavaScript would move integer-like keys first.
- A config candidate path that is a directory is treated as absent rather than as a fatal read
  error.
- The `{file:}` `~/` expansion uses `dirs::home_dir()`, which is the `oc-util` behavior.

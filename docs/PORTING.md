# opencode v2 → Rust port ("ocd"): working rules

Upstream reference: anomalyco/opencode branch `v2`, commit `4617210822bdbb31e5749751afbfb4584ccedac8` (MIT).
Read-only checkout: `$OCD_SCRATCH/src/opencode`
Specs: `docs/spec/01..08-*.md`. Verbatim assets: `assets/` (prompts, themes, models.dev snapshot, openapi.json).

## SAFETY: never run ocd against the user's real data
The user has opencode **1.18.31 (v1)** installed with real data in `~/.local/share/opencode/opencode.db` (505 sessions).
ocd uses the same paths by design. **Every** manual or scripted run of an ocd binary (or upstream v2 build) MUST set
isolated `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` (and `OPENCODE_TEST_HOME`), and prefer
`--standalone` (plain `ocd` starts a long-lived background service on port 49374). Incident 2026-10-12: a stray
`target/release/opencode serve --service` with real dirs applied the 11 newest v2 migrations to the real DB (v1 still
works on it — verified on a copy; backup of the post-incident state in `~/.local/share/opencode-backup-20261012-2315/`).

## Crates and owners
| crate | purpose | spec |
|---|---|---|
| oc-util | paths, jsonc, `{env:}`/`{file:}`, logging, flock, hash, version | 01 |
| oc-schema | IDs, domain types, messages, durable events | 02 |
| oc-storage | SQLite (same DB file as upstream), migrations, projectors | 02 |
| oc-ai | normalized LLM API, protocol adapters, models.dev catalog, provider registry | 05 |
| oc-tools | built-in tools (read/edit/write/patch/grep/glob/shell/webfetch/websearch/...) | 04 |
| oc-codemode | JS-subset interpreter behind the `execute` tool | 08 §4, 04 |
| oc-mcp | MCP client (stdio + streamable HTTP), OAuth, tools/resources/prompts | 08 §1 |
| oc-config | location config: discovery, JSONC+vars, v1→v2 normalize, per-key folds, markdown agents/commands/skills, cli.json | 01 §6 |
| oc-git | git ops: repo discovery, snapshots (capture/files/diff/restore/pack), worktrees, repository cache | 02 §6, 08 §5 |
| oc-backend | implements oc_server::Backend over oc-core (HTTP endpoints → core services) | 06 |
| oc-persistent-pty | client for the external `opencode-pty` daemon (binary resolution/download, socket protocol v7, ownership/handoff, terminals API) | 08 §2.5–2.9 |
| oc-core | config, agents, permissions, session loop, compaction, MCP, PTY | 01 §6, 03, 08 |
| oc-server | HTTP API (wire-compatible with `assets/openapi.json`) + SSE | 06 |
| oc-tui | ratatui client of the HTTP API | 07 |
| ocd | CLI binary `ocd` (crates/ocd) | 01 |

## Rules
- Rust 1.99, edition 2024. Use `export PATH=$HOME/.cargo/bin:$PATH`.
- Build/test ONLY your crate: `cargo check -p <crate>`, `cargo test -p <crate>`. Never run workspace-wide builds while others work.
- Do not edit the root `Cargo.toml` or other crates. Add deps to your crate's `Cargo.toml` (prefer `x.workspace = true` when present in the root; otherwise an explicit version).
- Behavior must match upstream exactly: user-visible strings, model-visible strings (tool descriptions, prompts, errors), JSON shapes and key order, defaults, limits. When the spec is unclear, read the upstream TS source.
- Prefer `include_str!` of files in `assets/` over copying large text into Rust.
- Write unit tests for algorithms (matching, parsing, truncation, framing).
- No git commits.

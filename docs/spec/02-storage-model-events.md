# 02 — Storage, Data Model, IDs, Event Bus, Snapshots

Spec for porting opencode v2 storage to a Rust replica that opens and reads/writes **the same SQLite
database file** as the TypeScript original (`@opencode/core` 2.0.26, commit `4617210`).

Source root for all citations: `packages/` in the opencode checkout. Citations use `path:line`.
Strings in `"…"` / code blocks are quoted verbatim from the source.

---

## 0. Conventions used by the original that matter for byte-compatibility

| Concern | Rule (TS original) | Source |
|---|---|---|
| JSON columns | Drizzle `text({ mode: "json" })`: written with `JSON.stringify(value)` (compact, no spaces), read with `JSON.parse`. `undefined` object keys are dropped by `JSON.stringify`. | `core/src/session/sql.ts:33,43,44,51,52,54,90` |
| Boolean columns | Drizzle `integer({ mode: "boolean" })`: stored `1`/`0`. | `core/src/session/sql.ts:156`, `core/src/credential/sql.ts:12` |
| Timestamps | All times are epoch **milliseconds** (`Date.now()`), INTEGER. | `core/src/database/schema.sql.ts:3-10` |
| `Timestamps` helper | `time_created`: `$default(() => Date.now())` (applied on INSERT when not given). `time_updated`: `$onUpdate(() => Date.now())` — Drizzle applies it on every `UPDATE … SET` that does **not** name `time_updated`, and also on INSERT when not given. Code that wants to *not* bump it writes `time_updated = time_updated` (`sql\`${SessionTable.time_updated}\``). | `core/src/database/schema.sql.ts:3-10`; e.g. `core/src/session/store.ts:212` |
| Schema-encoded JSON | Domain objects are encoded with Effect `Schema.encodeSync(...)` before storage. Optional fields that are `undefined` are **omitted** (helper `optional` filters `undefined`). `DateTimeUtcFromMillis` fields encode as a **number** (epoch ms). Branded strings encode as plain strings. | `schema/src/schema.ts:16-22,34-39` |
| JSON key order | Encoded objects follow the **schema field declaration order** (Effect builds struct output by iterating property signatures; a key re-declared after a spread keeps its first position). The Rust port should emit keys in the order listed in §4 to stay byte-identical (readers never depend on order). | see §4 |
| Paths | `directoryColumn`/`absoluteColumn`/`pathColumn`: on Windows `\` is replaced by `/` before storage; on read, Windows converts `X:/…`/`//…` back to `\`. Non-absolute values throw `Path is not absolute: <input>` (empty string allowed only for `session_v2.directory`). `absoluteArrayColumn` stores `JSON.stringify(string[])`. | `core/src/database/path.ts:1-95` |

---

## 1. Database location, connection, pragmas

### 1.1 File path

- Global data dir: `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode` (`util/src/global-roots.ts:5,13`,
  `util/src/global.ts:12-13`). Created with `mkdir -p` on startup together with config/state/log/bin/repos/tmp
  (`util/src/global.ts:62-70`).
- File name (`cli/src/database-path.ts:4-13`):
  ```ts
  const filename =
    process.env.OPENCODE_DB ??
    (["latest", "dev", "beta", "next", "prod"].includes(OPENCODE_CHANNEL) ||
    process.env.OPENCODE_DISABLE_CHANNEL_DB === "1" ||
    process.env.OPENCODE_DISABLE_CHANNEL_DB === "true"
      ? "opencode.db"
      : `opencode-${OPENCODE_CHANNEL.replace(/[^a-zA-Z0-9._-]/g, "-")}.db`)
  return filename === ":memory:" ? filename : path.resolve(data, filename)
  ```
  `OPENCODE_CHANNEL` is a build-time constant, `"local"` for unbuilt/dev runs (`cli/src/version.ts:6`), so a
  dev build uses `opencode-local.db`. Released builds use `opencode.db`.
- `Database.layer` (`core/src/database/database.ts:68-80`): `":memory:"` is in-memory; otherwise a relative
  path is joined onto `Global.data`.
- Sidecars: `<db>-wal`, `<db>-shm` (WAL mode).
- Legacy files read by migrations: `<data>/auth.json` (§2.9), `<data>/opencode-next.db` (§2.8).

### 1.2 File permissions (POSIX only)

`restrictToOwner` (`core/src/database/database.ts:85-99`): if the DB file is missing it is created
synchronously with `open(file, "a", 0o600)`; then `chmod 0600` on the DB and on `-wal`/`-shm` if they exist
(ENOENT ignored). Skipped on Windows.

### 1.3 Connection & pragmas

- Bun driver opens `new Database(filename, { readwrite: true, create: true })` and immediately runs
  `PRAGMA journal_mode = WAL;` (`core/src/database/sqlite.bun.ts:84-95`). Node uses `node:sqlite` `DatabaseSync`.
- Then, in order (`core/src/database/database.ts:39-47`):
  ```sql
  PRAGMA journal_mode = WAL
  PRAGMA synchronous = NORMAL
  PRAGMA busy_timeout = 5000
  PRAGMA cache_size = -64000
  PRAGMA wal_checkpoint(PASSIVE)
  PRAGMA foreign_keys = ON
  ```
- Then the migration step runs under a per-file semaphore (`database.ts:48-49,56-66`).
- **One connection per process**, guarded by a semaphore (all statements serialized)
  (`core/src/database/sqlite.ts:66-73`).
- Transactions: Drizzle-effect emits `begin deferred` by default, `begin immediate` when
  `{ behavior: "immediate" }` is passed (all durable event commits, §5.3), nested ones use
  `savepoint effect_sql_<n>` / `release savepoint …` / `rollback to savepoint …`
  (`core/src/database/drizzle/effect-sqlite/session.ts:168-193`).
- Integers are read as JS numbers (not BigInt) unless `SafeIntegers` is set (`sqlite.bun.ts:30-31`).

---

## 2. Migrations

### 2.1 Tracking table

```sql
CREATE TABLE `migration` (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)
```
(`core/src/database/migration.ts:38,57`; on upgrade paths it is `CREATE TABLE IF NOT EXISTS`). One row per
applied migration id; `time_completed` = `Date.now()` at apply time. There is no checksum column.

### 2.2 Bootstrap algorithm (`DatabaseMigration.apply`, `core/src/database/migration.ts:22-52`)

1. `SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND substr(name, 1, 1) <> '_'`
   (underscore-prefixed tables belong to embedders and are ignored).
2. If any of those is named `session` **or** `session_v2` → **upgrade path** `applyOnly(db, migrations)` (§2.3).
3. Else if the list is non-empty → fatal: `"Database is not empty and has no session table"`.
4. Else (**fresh DB**) in one transaction: run the full current schema DDL (`schema.gen.ts`, §2.5), create
   `migration`, and insert **every** migration id from §2.4 with `time_completed = Date.now()`. No individual
   migration bodies run on a fresh DB (so no `auth.json` import on fresh installs).

### 2.3 Upgrade path (`applyOnly`, `migration.ts:54-147`)

1. `CREATE TABLE IF NOT EXISTS migration (...)`; load the set of completed ids.
2. If that set is **empty** and table `__drizzle_migrations` exists (pre-TS-migration installs):
   - if `__drizzle_migrations` has a `name` column → `INSERT OR IGNORE INTO migration (id, time_completed) SELECT name, <now> FROM __drizzle_migrations WHERE name IS NOT NULL`;
   - else for each row `SELECT created_at, strftime('%Y%m%d%H%M%S', created_at / 1000, 'unixepoch') AS prefix … WHERE created_at IS NOT NULL`
     find the migration whose id starts with `${prefix}_`; if none → fatal
     `"Legacy migration timestamp <created_at> does not match any known migration"`; insert it.
   - reload completed set.
3. For each migration in registry order whose id is not completed: in **one transaction** (`begin deferred`)
   run its body then `INSERT INTO migration (id, time_completed) VALUES (?, Date.now())`.
   If `foreignKeys === false` (only `20260823191254_nullable_workspace_binding`), run
   `PRAGMA foreign_keys = OFF` before the transaction and `PRAGMA foreign_keys = ON` after (on Durable Objects
   `PRAGMA defer_foreign_keys = ON` instead).
4. Unknown ids present in `migration` (e.g. from a newer build or from the old "pre-split V2" lineage) are ignored.

> Note: several bodies execute `PRAGMA foreign_keys=OFF;` *inside* the migration transaction; SQLite ignores
> that pragma inside a transaction, so those are effectively no-ops. A Rust port must reproduce the effects,
> not rely on the pragma.

### 2.4 Ordered migration registry (`core/src/database/migration.gen.ts:1-102`)

| # | id | Effect (summary) |
|---|---|---|
| 0 | `20260127222353_familiar_lady_ursula` | Creates v1 tables `project`, `message`, `part`, `permission` (v1: `project_id` PK + `data`), `session` (v1), `todo`, `session_share`; indexes `message_session_idx`, `part_message_idx`, `part_session_idx`, `session_project_idx`, `session_parent_idx`, `todo_session_idx`. |
| 1 | `20260211171708_add_project_commands` | `ALTER TABLE project ADD commands text` |
| 2 | `20260213144116_wakeful_the_professor` | `control_account` table |
| 3 | `20260225215848_workspace` | v1 `workspace(id, branch, project_id NOT NULL FK, config NOT NULL)` |
| 4 | `20260227213759_add_session_workspace_id` | `session.workspace_id` + `session_workspace_idx` |
| 5 | `20260228203230_blue_harpoon` | `account` (with `selected_org_id`), `account_state(id integer PRIMARY KEY NOT NULL, active_account_id FK ON DELETE set null)` |
| 6 | `20260303231226_add_workspace_fields` | workspace `type NOT NULL`, `name`, `directory`, `extra`; drop `config` |
| 7 | `20260309230000_move_org_to_state` | `account_state.active_org_id` (copied from `account.selected_org_id`), drop `account.selected_org_id` |
| 8 | `20260312043431_session_message_cursor` | drop `message_session_idx`, `part_message_idx`; add `message_session_time_created_id_idx(session_id,time_created,id)`, `part_message_id_id_idx(message_id,id)` |
| 9 | `20260323234822_events` | `event_sequence(aggregate_id PK, seq)`, `event(id, aggregate_id FK→event_sequence CASCADE, seq, type, data)` |
| 10 | `20260410174513_workspace-name` | rebuild workspace with `name text DEFAULT '' NOT NULL` |
| 11 | `20260413175956_chief_energizer` | `session_entry` (later dropped) |
| 12 | `20260423070820_add_icon_url_override` | `project.icon_url_override`; copy from `icon_url` (see open question Q3) |
| 13 | `20260427172553_slow_nightmare` | create `session_message(id,session_id FK→session,type,time_created,time_updated,data)`; drop `session_entry` |
| 14 | `20260428004200_add_session_path` | `session.path` |
| 15 | `20260501142318_next_venus` | `session.agent`, `session.model` |
| 16 | `20260504145000_add_sync_owner` | `event_sequence.owner_id` |
| 17 | `20260507164347_add_workspace_time` | `workspace.time_used integer NOT NULL DEFAULT 0` |
| 18 | `20260510033149_session_usage` | `session.cost real`, `tokens_*` (DEFAULT 0 NOT NULL), backfilled from assistant `message.data` |
| 19 | `20260511000411_data_migration_state` | `data_migration(name PK, time_completed)` |
| 20 | `20260511173437_session-metadata` | `session.metadata` (skipped if column exists) |
| 21 | `20260601010001_normalize_storage_paths` | Windows `\` → `/` in `project.worktree/sandboxes`, `session.directory/path` |
| 22 | `20260601202201_amazing_prowler` | `DROP TABLE permission` |
| 23 | `20260602002951_lowly_union_jack` | new `permission(id, project_id, action, resource, times)` + unique idx |
| 24 | `20260602182828_add_project_directories` | `project_directory(project_id, directory, type NOT NULL, time_created)` |
| 25 | `20260603001617_session_message_projection_indexes` | event/session_message indexes |
| 26 | `20260603040000_session_message_projection_order` | `DELETE FROM session_message`; add `seq integer NOT NULL`; seq indexes |
| 27 | `20260603141458_session_input_inbox` | `session_input` (later dropped) |
| 28 | `20260603160727_jittery_ezekiel_stane` | `event_aggregate_type_seq_idx`, more indexes |
| 29 | `20260604172448_event_sourced_session_input` | wipes `session_input`, `session_message`, `event`, `event_sequence`, `workspace`; unique `event_aggregate_seq_idx`, `session_message_session_seq_idx`; rebuild `session_input` |
| 30 | `20260605003541_add_session_context_snapshot` | `session_context_epoch` (later dropped) |
| 31 | `20260605042240_add_context_epoch_agent` | — |
| 32 | `20260611035744_credential` | `credential` v0 |
| 33 | `20260611192811_lush_chimera` | drop/recreate `credential` in final shape |
| 34 | `20260612174303_project_dir_strategy` | `project_directory.type` nullable, add `strategy` |
| 35 | `20260622142730_simplify_session_context_epoch` | drop columns |
| 36 | `20260622170816_reset_v2_session_state` | wipe `session_context_epoch`, `session_input`, `session_message`, `event`, `event_sequence` |
| 37 | `20260622202450_simplify_session_input` | same wipe + `session.workspace_id = NULL`, `DELETE FROM workspace` |
| 38 | `20260804233008_loose_psylocke` | **V2 split**: creates `kv`, `instruction_blob`, `instruction_entry`, `instruction_state`, `session_pending`, `session_v2`; `ALTER TABLE event ADD created integer DEFAULT 0 NOT NULL`; recreate `session_message` with FK→`session_v2`; all v2 indexes; drop `data_migration`, `session_context_epoch`, `session_input`. Alternate branch if migration id `20260730195856_optional_session_title` is present: verify no V1-only history (`"Previous V2 database contains V1-only session history"`), rename `session`→`session_v2` and rebuild its indexes. |
| 39 | `20260805200742_import_legacy_credentials` | import `<data>/auth.json` (§2.9) |
| 40 | `20260808023530_workspace_domain` | drop+recreate `workspace(id, provider, binding NOT NULL, created_at, last_used_at)` |
| 41 | `20260811161259_execution_claim_attempts` | `session_v2.resume_attempts integer DEFAULT 0 NOT NULL` |
| 42 | `20260812181746_session_inbox` | `session_inbox` + indexes |
| 43 | `20260812213948_worktree` | `worktree` table, backfilled from `project_directory` (`strategy='git_worktree'`→`'git'`; else strategy; else `type='git_worktree'`→`'git'`) |
| 44 | `20260819222447_session_viewed_state` | `session_v2.time_idle`, `time_viewed`, `idle_outcome` |
| 45 | `20260823191254_nullable_workspace_binding` | `foreignKeys: false`; rebuild `workspace` with `binding` nullable |
| 46 | `20260910120000_clear_v1_session_permission` | `UPDATE session_v2 SET permission = NULL` |
| 47 | `20260923013825_project_time_active` | `project.time_active integer DEFAULT 0 NOT NULL`; `= time_updated` |
| 48 | `20261007190000_azure_cli_external_credential` | `credential.value`: `json_remove(json_set(value,'$.type','external'),'$.access','$.refresh','$.expires')` where `integration_id='azure'` and `$.type='oauth'` and `$.methodID='azure-cli'` |

A database is "fully migrated" for this version iff `migration` contains all 49 ids above.

### 2.5 Final schema (fresh bootstrap DDL, verbatim from `core/src/database/schema.gen.ts:7-277`)

Executed in this order inside one transaction, followed by the `migration` table (§2.1).

```sql
CREATE TABLE `account_state` (
  `id` integer PRIMARY KEY,
  `active_account_id` text,
  `active_org_id` text,
  CONSTRAINT `fk_account_state_active_account_id_account_id_fk` FOREIGN KEY (`active_account_id`) REFERENCES `account`(`id`) ON DELETE SET NULL
);
CREATE TABLE `account` (
  `id` text PRIMARY KEY,
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `control_account` (
  `email` text NOT NULL,
  `url` text NOT NULL,
  `access_token` text NOT NULL,
  `refresh_token` text NOT NULL,
  `token_expiry` integer,
  `active` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `control_account_pk` PRIMARY KEY(`email`, `url`)
);
CREATE TABLE `credential` (
  `id` text PRIMARY KEY,
  `integration_id` text,
  `label` text NOT NULL,
  `value` text NOT NULL,
  `connector_id` text,
  `method_id` text,
  `active` integer,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `event_sequence` (
  `aggregate_id` text PRIMARY KEY,
  `seq` integer NOT NULL,
  `owner_id` text
);
CREATE TABLE `event` (
  `id` text PRIMARY KEY,
  `aggregate_id` text NOT NULL,
  `seq` integer NOT NULL,
  `created` integer DEFAULT 0 NOT NULL,
  `type` text NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_event_aggregate_id_event_sequence_aggregate_id_fk` FOREIGN KEY (`aggregate_id`) REFERENCES `event_sequence`(`aggregate_id`) ON DELETE CASCADE
);
CREATE TABLE `kv` (
  `key` text PRIMARY KEY,
  `value` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL
);
CREATE TABLE `permission` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `action` text NOT NULL,
  `resource` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `fk_permission_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project_directory` (
  `project_id` text NOT NULL,
  `directory` text NOT NULL,
  `type` text,
  `strategy` text,
  `time_created` integer NOT NULL,
  CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
  CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `project` (
  `id` text PRIMARY KEY,
  `worktree` text NOT NULL,
  `vcs` text,
  `name` text,
  `icon_url` text,
  `icon_url_override` text,
  `icon_color` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_initialized` integer,
  `time_active` integer DEFAULT 0 NOT NULL,
  `sandboxes` text NOT NULL,
  `commands` text
);
CREATE TABLE `instruction_blob` (
  `hash` text PRIMARY KEY,
  `value` text
);
CREATE TABLE `instruction_entry` (
  `session_id` text NOT NULL,
  `key` text NOT NULL,
  `value` text,
  `removed` integer DEFAULT false NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  CONSTRAINT `instruction_entry_pk` PRIMARY KEY(`session_id`, `key`),
  CONSTRAINT `fk_instruction_entry_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE TABLE `instruction_state` (
  `session_id` text PRIMARY KEY,
  `epoch_start` integer NOT NULL,
  `through_seq` integer NOT NULL,
  `initial_values` text NOT NULL,
  `current_values` text NOT NULL,
  CONSTRAINT `fk_instruction_state_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_inbox` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `payload` text NOT NULL,
  `delivery` text NOT NULL,
  `enqueued_seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  CONSTRAINT `fk_session_inbox_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `fk_session_message_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_pending` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `data` text NOT NULL,
  `delivery` text,
  `admitted_seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  CONSTRAINT `fk_session_pending_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
);
CREATE TABLE `session_v2` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,
  `fork_session_id` text,
  `fork_boundary` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `path` text,
  `title` text,
  `version` text NOT NULL,
  `share_url` text,
  `summary_additions` integer,
  `summary_deletions` integer,
  `summary_files` integer,
  `summary_diffs` text,
  `metadata` text,
  `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL,
  `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
  `revert` text,
  `permission` text,
  `agent` text,
  `model` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_idle` integer,
  `time_viewed` integer,
  `idle_outcome` text,
  `time_compacting` integer,
  `time_archived` integer,
  `time_suspended` integer,
  `resume_attempts` integer DEFAULT 0 NOT NULL,
  CONSTRAINT `fk_session_v2_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE TABLE `workspace` (
  `id` text PRIMARY KEY,
  `provider` text NOT NULL,
  `binding` text,
  `created_at` integer NOT NULL,
  `last_used_at` integer NOT NULL
);
CREATE TABLE `worktree` (
  `project_id` text NOT NULL,
  `directory` text NOT NULL,
  `strategy` text,
  `time_created` integer NOT NULL,
  CONSTRAINT `worktree_pk` PRIMARY KEY(`project_id`, `directory`),
  CONSTRAINT `fk_worktree_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
CREATE UNIQUE INDEX `event_aggregate_seq_idx` ON `event` (`aggregate_id`,`seq`);
CREATE INDEX `event_aggregate_type_seq_idx` ON `event` (`aggregate_id`,`type`,`seq`);
CREATE UNIQUE INDEX `permission_project_action_resource_idx` ON `permission` (`project_id`,`action`,`resource`);
CREATE INDEX `session_inbox_session_delivery_seq_idx` ON `session_inbox` (`session_id`,`delivery`,`enqueued_seq`);
CREATE UNIQUE INDEX `session_inbox_session_enqueued_seq_idx` ON `session_inbox` (`session_id`,`enqueued_seq`);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);
CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);
CREATE INDEX `session_pending_session_delivery_seq_idx` ON `session_pending` (`session_id`,`delivery`,`admitted_seq`);
CREATE UNIQUE INDEX `session_pending_session_compaction_idx` ON `session_pending` (`session_id`) WHERE "session_pending"."type" = 'compaction';
CREATE UNIQUE INDEX `session_pending_session_admitted_seq_idx` ON `session_pending` (`session_id`,`admitted_seq`);
CREATE INDEX `session_v2_project_idx` ON `session_v2` (`project_id`);
CREATE INDEX `session_v2_workspace_idx` ON `session_v2` (`workspace_id`);
CREATE INDEX `session_v2_parent_idx` ON `session_v2` (`parent_id`);
CREATE INDEX `session_v2_time_suspended_idx` ON `session_v2` (`time_suspended`) WHERE "session_v2"."time_suspended" is not null;
```

Table purpose / liveness:

| Table | Used by v2 code? | Owner |
|---|---|---|
| `project`, `worktree`, `session_v2`, `session_message`, `session_inbox`, `instruction_*`, `event`, `event_sequence`, `kv`, `credential`, `permission`, `workspace` | yes | §4/§5 |
| `project_directory` | deprecated (read-only; `@deprecated Use WorktreeTable`) | `core/src/project/sql.ts:30-46` |
| `session_pending` | defined, no live readers/writers (superseded by `session_inbox`) | `core/src/session/sql.ts:100-123` |
| `account`, `account_state`, `control_account` | defined in `core/src/account/sql.ts`; no core consumers found | — |

### 2.6 Differences in a DB that reached this version through the upgrade path

A Rust port must tolerate these (always select columns **by name**, never `SELECT *` positionally):

- **Extra legacy (v1) tables remain**: `session` (v1), `message`, `part`, `todo`, `session_share`, and possibly
  `__drizzle_migrations`. Their indexes remain: `session_project_idx`, `session_parent_idx`, `session_workspace_idx`,
  `message_session_time_created_id_idx`, `part_session_idx`, `part_message_id_id_idx`, `todo_session_idx`.
  Final v1 `session` columns: `id, project_id, parent_id, slug, directory, title NOT NULL, version, share_url,
  summary_additions, summary_deletions, summary_files, summary_diffs, revert, permission, time_created,
  time_updated, time_compacting, time_archived, workspace_id, path, agent, model, cost, tokens_input,
  tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, metadata`.
  `message(id, session_id FK→session, time_created, time_updated, data)`;
  `part(id, message_id FK→message, session_id, time_created, time_updated, data)`;
  `todo(session_id, content, status, priority, position, time_created, time_updated, PK(session_id, position))`;
  `session_share(session_id PK, id, secret, url, time_created, time_updated)`.
- **Column order differs**: `event` has `created` last; `project` ends `…, sandboxes, commands, icon_url_override, time_active`;
  `session_v2` ends `…, time_compacting, time_archived, time_suspended, resume_attempts, time_idle, time_viewed, idle_outcome`.
- `account_state.id` is `integer PRIMARY KEY NOT NULL` and its FK is unnamed with `ON UPDATE no action ON DELETE set null`.
- If the DB came from the "pre-split V2" lineage (marker id `20260730195856_optional_session_title`), `session_v2`
  is the renamed old `session` table and `migration` contains ids not in §2.4.

### 2.7 Recommended Rust recognition algorithm

Mirror §2.2/§2.3 exactly: (1) same table probe; (2) fresh → run §2.5 DDL + insert all 49 ids; (3) otherwise
create `migration` if missing, seed from `__drizzle_migrations` if empty, then for each of the 49 ids missing,
run the port's copy of that migration body. Embed the 49 ids as constants. A DB with all 49 ids needs nothing.

### 2.8 V1 data import ("v1-migration", Bun builds only)

`core/src/database/v1-migration.bun.ts` (Node/workerd builds use a no-op, `core/package.json` `#v1-migration`).
Started in the background by the server (`server/src/routes.ts:188`); status exposed via
`V1Migration.status()` (`required | running{progress} | error | completed`).

- State in KV key `"migration.v1-v2"` (`v1-migration.bun.ts:215`): `{"phase":"sessions"}`,
  `{"phase":"sessions","cursor":"<v1 session id>"}`, or `{"phase":"completed"}`.
- Only runs if a table named `session` exists and state ≠ completed.
- Ensures the global project row: `INSERT OR IGNORE INTO project (id, worktree, time_created, time_updated, time_active, sandboxes) VALUES ('global', <root of data dir>, now, now, now, '[]')` (`:544`).
- On first run (no state): delete **all** rows from `event` in batches of 1000, then set state `{"phase":"sessions"}`.
- Imports `<data>/opencode-next.db` (pre-split V2 DB, if present and has `project`, `session`, `session_message`):
  copies projects (`INSERT OR IGNORE`), sessions into `session_v2` (skip existing), messages verbatim, and sets
  `event_sequence.seq` to the last message seq (or -1), `owner_id = NULL` (`:729-837`).
- Then iterates v1 `session` rows by `id DESC` after the cursor, one transaction per session: update cursor;
  `INSERT OR IGNORE INTO session_v2 … SELECT … FROM session` (permission forced NULL; missing project →
  `'global'`); transforms v1 `message`+`part` rows into v2 `session_message` rows (`transformSession`, `:222-491`:
  user/synthetic/assistant/compaction/system messages, seq = 0..n-1; tool renames
  `{ bash: "shell", task: "subagent", apply_patch: "patch" }`, `todowrite` removed, `:899-901`); recomputes
  cost/tokens/agent/model; upserts `event_sequence(aggregate_id=session id, seq=last seq, owner_id NULL)`.
- Finally state `{"phase":"completed"}`. v1 tables are **not** dropped.

### 2.9 Legacy credential import (migration 39)

Reads `<data>/auth.json` (missing → no-op; non-object → error `"Legacy credential file must contain an object"`).
For each `[id, value]` (`core/src/database/migration/20260805200742_import_legacy_credentials.ts`):
integrationID = `id` with trailing `/` stripped; skip if a `credential` row with that `integration_id` exists.
- `{"type":"api","key","metadata"?}` → `{"type":"key","key":…, "metadata"?}` label `"API key"`
- `{"type":"wellknown","key","token"}` → `{"type":"key","key":<token>}` label `"API key"`; origin recorded
- `{"type":"oauth","refresh","access","expires","accountId"?,"enterpriseUrl"?}` → `{"type":"oauth","methodID":M,"refresh","access","expires","metadata"?:{"accountID"?,"enterpriseUrl"?}}` label `"OAuth"`; `M = "chatgpt-browser"` for `openai`, `"device"` for `github-copilot|opencode|xai`, else `"oauth"`.
Insert `(id=cred_…, integration_id, label, value=JSON, time_created=time_updated=now)` (`active` NULL).
Wellknown origins are merged (de-duplicated, order-preserving) into KV `"wellknown:sources"` (JSON string array).

---

## 3. ID generation

### 3.1 Core algorithm (`schema/src/identifier.ts:1-32`, verbatim)

```ts
const length = 26
const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
let lastTimestamp = 0
let counter = 0

export function create(descending: boolean, timestamp = Date.now()) {
  if (timestamp !== lastTimestamp) {
    lastTimestamp = timestamp
    counter = 0
  }
  counter++

  const current = BigInt(timestamp) * 0x1000n + BigInt(counter)
  const value = descending ? ~current : current
  const time = Array.from({ length: 6 }, (_, index) =>
    Number((value >> BigInt(40 - 8 * index)) & 0xffn)
      .toString(16)
      .padStart(2, "0"),
  ).join("")
  const bytes = crypto.getRandomValues(new Uint8Array(length - 12))
  return time + Array.from(bytes, (byte) => chars[byte % 62]).join("")
}
```

Rust equivalent (exact):
```
counter: process-global, shared by ALL id kinds; reset to 0 whenever ms timestamp != last; then counter += 1
current = (ts_ms as u64) << 12 + counter          // wrapping add; counter may exceed 4095
v48     = current & 0xFFFF_FFFF_FFFF              // only the low 48 bits survive
v48     = if descending { 0xFFFF_FFFF_FFFF - v48 } else { v48 }   // == (!current) & mask
body    = lowercase_hex(v48, 12 digits) + 14 chars, each = ALPHABET[random_u8 % 62]
```
- Body is always 26 chars: 12 lowercase hex + 14 base62 (`0-9A-Za-z`, modulo-biased).
- **Truncation**: `ts_ms << 12` is ~53 bits today, so only `ts_ms mod 2^36` (~795 days) survives — the
  lexical order of IDs wraps roughly every 2.18 years. This is the actual behavior; replicate it
  (verified: `ts=1760000000000, counter=1` → ascending `9c82cc000001`, descending `637d33fffffe`).
- Counter resets when the clock moves backwards too (it only checks `!==`).

### 3.2 Prefixes

| Kind | Format | Order | Source |
|---|---|---|---|
| Session | `ses_` + descending | newest sorts first | `schema/src/session-id.ts:9` |
| Session message / inbox item | `msg_` + ascending; or `fromEvent`: event id with `evt_`→`msg_` | asc | `schema/src/session-message.ts:23-30` |
| Forked message copy | `` `${msg_<forked-event-suffix>}_${seq}` `` | — | `core/src/session/projector.ts:218` |
| Event | `evt_` + ascending | asc | `schema/src/event.ts:9-13` |
| Workspace | `wrk_` + ascending | asc | `schema/src/workspace-id.ts` |
| Permission request | `per_` + ascending | asc | `schema/src/permission.ts:10-13` |
| Saved permission | `psv_` + ascending | asc | `schema/src/permission-saved.ts:11` |
| Credential | `cred_` + ascending | asc | `schema/src/credential.ts:14` |
| Connection | `con_` + ascending | asc | `schema/src/integration.ts:86` |
| Shell | `sh_` + ascending | asc | `schema/src/shell.ts:9-16` |
| PTY | `pty_` + ascending | asc | `schema/src/pty.ts:9-16` |
| Form | `frm_` + ascending | asc | `schema/src/form.ts:8-14` |
| Tool output file | `tool_` + ascending (file name) | asc | `core/src/tool-output.ts:18` |
| v1 message/part | `msg_`/`prt_` + ascending | asc | `schema/src/v1/session.ts:17-25` |

Validation is prefix-only (`isStartingWith("ses")`, `"msg_"`, `"evt_"`, `"wrk"`, `"per"`, `"sh_"`, `"pty"`, `"frm_"`).

### 3.3 Project ID (`core/src/project.ts:257-376`)

`Hash.fast(x)` = SHA-1 hex (`util/src/hash.ts`). Resolution of a directory:
1. Walk up for first `.git` or `.hg`.
2. Git: `git rev-parse --git-dir --git-common-dir --show-toplevel`; id =
   `sha1("git-remote:" + host.lower() + "/" + path)` from `git remote get-url origin` (URL or scp form; path
   stripped of leading `/`, trailing `.git`/`.git/`, trailing `/`; `file:` URLs ignored) → else trimmed
   contents of `<git-common-dir>/opencode` (legacy cache) → else first of sorted
   `git rev-list --max-parents=0 HEAD` → else `"global"`.
3. Hg: `<.hg>/opencode` cache → first sorted line of `hg log -r "roots(all())" -T "{node}\n"` (env `HGPLAIN=1`) → `"global"`.
4. No VCS: `sha1("directory:" + <resolved directory>)`.
Special id: `"global"` (`schema/src/project-id.ts:7`).

### 3.4 Other derived values

- Session `slug`: `"<adjective>-<noun>"` chosen with `Math.random()` from fixed lists (`core/src/util/slug.ts`).
- Instruction hash: SHA-256 hex of canonical JSON (keys sorted by UTF-16 code unit, `JSON.stringify` for
  scalars/keys, no whitespace) (`core/src/instructions/index.ts:194-196,243-251`).
- Snapshot ID: a git tree SHA (40 or 64 hex) (§6.1).

---

## 4. Data model (JSON shapes)

Notation: `?` = optional key (omitted when absent); `| null` = explicit null allowed; ms = epoch-millis number.

### 4.1 Common value types

| Type | JSON | Source |
|---|---|---|
| `Location.Ref` | `{"directory": AbsPath, "workspaceID"?: "wrk_…"}` | `schema/src/location.ts:9-12` |
| `Model.Ref` | `{"id": string, "providerID": string, "variant"?: string}` | `schema/src/model.ts:18-22` |
| `TokenUsage.Info` | `{"input": n, "output": n, "reasoning": n, "cache": {"read": n, "write": n}}` | `schema/src/token-usage.ts` |
| `Money.USD` | number | `schema/src/money.ts` |
| `SessionError.Error` | `{"type": string, "message": string, "status"?: int 100..599, "response"?: {"body": string}}` | `schema/src/session-error.ts` |
| `FileDiff.Info` | `{"file", "patch", "additions": int≥0, "deletions": int≥0, "status": "added"\|"deleted"\|"modified"}` | `schema/src/file-diff.ts` |
| `FileDiff.LegacyInfo` | `{"file"?, "patch"?, "additions", "deletions", "status"?}` | same |
| `Permission.Rule` / `Ruleset` | `[{"action": string, "resource": string, "effect": "allow"\|"deny"\|"ask"}]` | `schema/src/permission.ts:60-66` |
| `Tool.Content` | `{"type":"text","text"}` \| `{"type":"file","uri","mime","name"?}` | `schema/src/tool.ts` |
| `Prompt.FileAttachment` | `{"data": base64, "mime", "source": {"type":"inline"}\|{"type":"uri","uri"}, "name"?, "description"?, "mention"?: {"start","end","text"}}` | `schema/src/prompt.ts:26-34` |
| `Prompt.AgentAttachment` | `{"name", "mention"?}` | `prompt.ts:51-54` |
| `Prompt.SkillAttachment` | `{"id", "name", "text"?, "mention"?}` | `prompt.ts:57-62` |
| `Session.ForkBoundary` | `{"type":"before"\|"through","messageID":"msg_…"}` | `schema/src/session-fork.ts` |
| `Session.Revert` | `{"messageID", "partID"?, "snapshot"?, "files"?: FileDiff.Info[]}` | `schema/src/session-revert.ts:8-14` |
| `RevertV1` (read-compat) | `{"messageID","partID"?,"snapshot"?,"diff"?,"files"?:[{"path","status","additions","deletions","patch"}]}` → decoded to `Revert` with `path`→`file` (`PersistedRevert`, `:59-86`) | |
| `ProviderContext` | `{"version":1,"provenance":{"providerID","provider","modelID","route","protocol","endpoint"},"messages": <json>}` | `schema/src/session-provider-context.ts` |
| `Shell.Info` | `{"id":"sh_…","status":"running"\|"exited"\|"timeout"\|"killed","command","cwd","shell","file","pid"?,"exit"?,"signal"?,"metadata":{},"time":{"started":ms,"completed"?:ms}}` | `schema/src/shell.ts:40-55` |
| `Shell.Output` | `{"output": string, "cursor": int, "size": int, "truncated": bool}` | `shell.ts:77-84` |
| `LLM.FinishReason` | `"stop"\|"length"\|"tool-calls"\|"content-filter"\|"error"\|"unknown"` | `schema/src/llm.ts` |

### 4.2 Session

**API/export shape `Session.Info`** (`schema/src/session.ts:31-59`), key order:
```json
{
  "id": "ses_…",
  "parentID": "ses_…",
  "fork": { "sessionID": "ses_…", "boundary": { "type": "through", "messageID": "msg_…" } },
  "projectID": "…",
  "agent": "build",
  "model": { "id": "…", "providerID": "…", "variant": "default" },
  "cost": 0.0123,
  "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
  "outcome": "succeeded",
  "time": { "created": 1760000000000, "updated": 1760000000000, "idle": 0, "viewed": 0, "archived": 0 },
  "title": "…",
  "location": { "directory": "/abs", "workspaceID": "wrk_…" },
  "subpath": "pkg/a",
  "metadata": { },
  "permissions": [ ],
  "revert": { "messageID": "msg_…" }
}
```
Optional: `parentID, fork, agent, model, outcome ("succeeded"|"failed"|"interrupted"), time.idle, time.viewed,
time.archived, title, location.workspaceID, subpath, metadata, permissions, revert`.

**Row → Info mapping** (`core/src/session/info.ts:16-63`):

| Info | Column | Rule |
|---|---|---|
| `id` | `id` | |
| `projectID` | `project_id` | |
| `title` | `title` | NULL → absent |
| `parentID` | `parent_id` | NULL/empty → absent |
| `fork` | `fork_session_id`, `fork_boundary` (JSON) | only if both non-null |
| `agent` | `agent` | |
| `model` | `model` (JSON `{id, providerID, variant?}`) | `variant` defaults to `"default"` on read |
| `cost`, `tokens.*` | `cost`, `tokens_input/output/reasoning/cache_read/cache_write` | |
| `location` | `directory`, `workspace_id` | |
| `subpath` | `path` | empty/NULL → absent |
| `metadata` | `metadata` (JSON object) | |
| `permissions` | `permission` (JSON Ruleset) | |
| `revert` | `revert` (JSON, `PersistedRevert` decoder accepts v1 shape) | |
| `outcome` | `idle_outcome` | |
| `time.created/updated` | `time_created/time_updated` | |
| `time.idle/viewed` | `time_idle/time_viewed` | NULL → absent |
| `time.archived` | `time_archived` | 0/NULL → absent |

Columns not surfaced: `slug`, `version` (app version string at creation, `App.Metadata.version`, default
`"unknown"`), `share_url`, `summary_*` (legacy only), `time_compacting` (legacy), `time_suspended`
(execution claim timestamp — "historical column name", `core/src/session/sql.ts:65-66`), `resume_attempts`.

### 4.3 Session messages (`session_message` rows)

Row: `id` = message id, `session_id`, `type` = discriminator, `seq` = durable seq of the event that **inserted**
it, `time_created` = `message.time.created` (ms), `time_updated` = wall-clock ms of last write, `data` = the
schema-encoded message **minus `id` and `type`** (`core/src/session/projector.ts:395-410`). Readers
reconstruct with `{...data, id: row.id, type: row.type}` (`core/src/session/history.ts:62-63`).

Common `Base` (`schema/src/session-message.ts:33-37`): `id`, `metadata?: Record<string, unknown>`,
`time: {created: ms}`.

| `type` | Fields in `data` (in order) | Source |
|---|---|---|
| `agent-switched` | `metadata?`, `time{created}`, `agent`, `previous?` | `:44-50` |
| `model-switched` | `metadata?`, `time`, `model: Model.Ref`, `previous?: Model.Ref` | `:52-58` |
| `location-switched` | `metadata?`, `time`, `location: Location.Ref`, `projectID?`, `subpath?`, `previous?: {location, projectID?, subpath?}` | `:60-72` |
| `user` | `metadata?`, `time`, `text`, `files?: FileAttachment[]`, `agents?: AgentAttachment[]`, `skills?: SkillAttachment[]` | `:74-82` |
| `synthetic` | `metadata?`, `time`, `text`, `description?` | `:84-90` |
| `system` | `metadata?`, `time`, `text`, `description?` | `:92-100` |
| `skill` | `metadata?`, `time`, `skill` (Skill.ID), `name`, `text` | `:102-109` |
| `shell` | `metadata?`, `time{created, completed?}`, `shellID`, `command`, `status`, `exit?`, `output?: Shell.Output` | `:111-124` |
| `assistant` | `metadata?`, `time{created, streamed?, completed?}`, `agent`, `model`, `content: AssistantContent[]`, `snapshot?{start?, end?, files?: RelPath[]}`, `finish?`, `rawFinish?`, `providerState?`, `cost?`, `tokens?`, `error?: SessionError`, `retry?{attempt: int>0, at: ms, error}` | `:212-237` |
| `compaction` | `metadata?`, `time`, `status` + per status: `running`: `reason: "auto"\|"manual"`, `summary`, `recent`; `completed`: `reason`, `model?`, `providerState?`, `summary`, `recent`, `providerContext?`, `cost?`, `tokens?`; `failed`: `reason`, `error`, `cost?`, `tokens?` | `:239-282` |
| `idle` | `metadata?`, `time`, `outcome: "succeeded"\|"failed"\|"interrupted"`, `error?` | `:289-295` |

`AssistantContent` items (`:126-198`):
- `{"type":"text","text","state"?}`
- `{"type":"reasoning","text","state"?,"time"?:{"created":ms,"completed"?:ms}}`
- `{"type":"tool","id","name","executed"?,"providerState"?,"providerResultState"?,"state":ToolState,"time":{"created":ms,"ran"?:ms,"completed"?:ms}}`

`ToolState` by `status`:
- `streaming`: `{"status":"streaming","input": string}` (raw partial JSON)
- `running`: `{"status":"running","input":{…},"metadata":{…}}`
- `completed`: `{"status":"completed","input":{…},"content":[Content, …≥1],"metadata"?}`
- `error`: `{"status":"error","input":{…},"error":SessionError,"content"?,"metadata"?}`

Example rows:
```text
id=msg_19a2…  type=user  seq=3  data={"time":{"created":1760000000123},"text":"fix the bug"}
id=msg_19a2…  type=assistant seq=4
data={"time":{"created":1760000000200,"completed":1760000004000},"agent":"build",
      "model":{"id":"claude-x","providerID":"anthropic"},
      "content":[{"type":"text","text":"Done."},
                 {"type":"tool","id":"call_1","name":"read","executed":true,
                  "state":{"status":"completed","input":{"path":"a.ts"},"content":[{"type":"text","text":"…"}]},
                  "time":{"created":1760000001000,"ran":1760000001100,"completed":1760000001200}}],
      "finish":"stop","cost":0.01,
      "tokens":{"input":100,"output":20,"reasoning":0,"cache":{"read":0,"write":0}}}
```

There are **no separate "parts"** in v2: text/reasoning/tool are `content` items of assistant messages;
step boundaries are `session.step.*` events; snapshots live in `assistant.snapshot`; subagents are tool
calls named `subagent`; compaction is a message type. The v1 part model is in §4.12.

### 4.4 Session inbox (`session_inbox`) — pending input

`schema/src/session-inbox.ts`. Row: `id` (msg_…), `session_id`, `type` ∈ `user|synthetic|compaction|move`,
`payload` JSON, `delivery` ∈ `"steer"|"queue"`, `enqueued_seq` = seq of the `session.inbox.enqueued` event,
`time_created` = event `created`.

| type | payload JSON |
|---|---|
| `user` | `{"text","files"?,"agents"?,"skills"?,"metadata"?}` (`UserPayload` = Prompt fields + metadata) |
| `synthetic` | `{"text","description"?,"metadata"?}` |
| `compaction` | `{}` |
| `move` | `{"location": Location.Ref, "projectID", "subpath"?}` |

API `SessionInbox.Info` = `{"id","sessionID","time":{"created"},"type","payload","delivery"}`
(`session-inbox.ts:52-78`). Rows are **deleted** when delivered (promoted to a `session_message` with the same
id), cancelled, or when a revert commits past them.

### 4.5 Instructions

- `instruction_blob(hash, value)`: `hash` = SHA-256 of canonical JSON of `value` (§3.4), `value` JSON.
  Insert `ON CONFLICT DO NOTHING` (`core/src/session/instruction-state.ts:227-231`).
- `instruction_state(session_id, epoch_start, through_seq, initial_values, current_values)`:
  values JSON = `{"<source>/<key>": "<sha256>"}` (`Instruction.Key` pattern
  `^[a-z0-9][a-z0-9._-]*\/[a-z0-9][a-z0-9._/-]*$`).
  - `apply(seq, delta)`: delta `{"key": "<hash>" | "removed"}`; first time inserts
    `epoch_start = through_seq = seq, initial = current = applied`; else `through_seq = seq, current_values = applied` (`:89-117`).
  - `advanceEpoch(seq)` on `session.compaction.ended`: `epoch_start = through_seq = seq, initial_values = current_values` (`:139-154`).
  - `reset` on revert commit: delete row. `initialize` on fork: insert `ON CONFLICT DO NOTHING` at the fork event seq (0).
- `instruction_entry(session_id, key, value, removed, …)`: API-managed context entries (key pattern
  `^[a-z0-9][a-z0-9._-]*$`, value ≤ 256 KiB JSON, error `"Instruction entry value is <n> bytes; the limit is 262144 bytes"`).
  `put` upserts (`removed=0`, `time_updated=Date.now()`) only when value changed or row was removed; `remove`
  sets `value=NULL, removed=1` (`core/src/session/instruction-entry.ts:120-160`). Exposed to the
  instruction system as key `api/<key>`.

### 4.6 Project (`project`)

`Project.Info` (`schema/src/project.ts:39-48`):
```json
{"id":"…","canonical":"/abs/repo","vcs":"git","name":"…",
 "icon":{"url":"…","override":"…","color":"…"},"commands":{"start":"…"},
 "time":{"created":ms,"updated":ms,"active":ms},"sandboxes":["/abs"]}
```
Row mapping (`core/src/project.ts:73-96`): `canonical`=`worktree`; `icon` present iff any of `icon_url`,
`icon_url_override`, `icon_color`; `sandboxes` = JSON string array. `vcs` ∈ `"git"|"hg"` (pattern `^[a-z][a-z0-9._-]*$`).
- Upsert on resolve (`core/src/project/sql.ts:48-62`): `INSERT (id, worktree, vcs, sandboxes='[]') ON CONFLICT(id) DO UPDATE SET vcs = ? WHERE` (vcs given: `vcs IS NULL OR vcs <> ?`; vcs absent: `vcs IS NOT NULL`).
- Canonical is replaced only if the previously stored directory no longer exists (`project.ts:116-130`).
- `activate`: at most once per 60 s per project, `time_active = now`, `time_updated` unchanged (`:241-255`).
- `list` order: `time_active DESC, id ASC`.
- Update: empty strings clear (`name`, `icon_url_override`, `icon_color` → NULL; `commands` → NULL if `start` empty).

### 4.7 Worktree (`worktree`) and `project_directory`

`worktree(project_id, directory, strategy, time_created)`: one row per (project, directory) known to the
project; `strategy` `"git"` for a secondary git worktree, else NULL. Inserted `ON CONFLICT DO NOTHING` in the
same transaction as the durable `worktree.resolved` event (`core/src/project.ts:182-199`).
`project_directory` is legacy (backfill source only).

### 4.8 Workspace (`workspace`)

`Workspace.Info` (`core/src/workspace.ts:20-26`): `{"id":"wrk_…","provider":string,"binding":{…},"createdAt":ms,"lastUsedAt":ms}`.
Row: `binding` JSON object (driver-owned, `Record<string, Json>`) or NULL until provisioned. Created with
`binding NULL, created_at = last_used_at = now` (`:241-246`); `last_used_at` updated on connect/idle suspend.

### 4.9 Permissions

- Saved permission rows (`permission`): `PermissionSaved.Info` = `{"id":"psv_…","projectID","action","resource","time":{"created","updated"}}`;
  `add` inserts one row per resource `ON CONFLICT DO NOTHING` (unique `(project_id, action, resource)`)
  (`core/src/permission/saved.ts:63-78`).
- Session ruleset: `session_v2.permission` JSON `Permission.Ruleset` ("Evaluated after the agent's rules; the
  last matching rule wins").
- `Permission.Request` (ephemeral, not stored): `{"id":"per_…","sessionID","action","resources":[…],"save"?:[…],"metadata"?,"source"?:{"type":"tool","messageID","id"},"message"?}`; replies `"once"|"always"|"reject"`.

### 4.10 Credential (`credential`)

Row: `id` (`cred_…`), `integration_id` (NULL rows are ignored by readers), `label` (default `"default"`),
`value` JSON `Credential.Value`, `connector_id`/`method_id` (legacy, unused), `active` (1/0/NULL).

`Credential.Value` (`schema/src/credential.ts:33-60`):
- `{"type":"oauth","methodID","refresh","access","expires":int≥0,"metadata"?}`
- `{"type":"key","key","metadata"?,"configuration"?}`
- `{"type":"external","methodID","metadata"?}`

Selection: per integration the selected row is the first by `active DESC, time_created DESC, id DESC`
(legacy rows with NULL active → newest wins). `create` with `activate !== false` (or no existing) sets all
the integration's rows `active=0` then inserts `active=1`; with `activate=false` and an existing unpinned
current row, it first pins that row `active=1` (`core/src/credential.ts:113-145`). Lists order by
`active ASC, time_created ASC, id ASC`.

### 4.11 KV (`kv`)

`core/src/kv.ts`: `value` is any JSON. `set` = `INSERT … ON CONFLICT(key) DO UPDATE SET value, time_updated = Date.now()`.
`scan({prefix, after?, limit?})`: limit clamped to `[1,1000]` (default 100; NaN→100), range
`key >= prefix AND key < prefixEnd(prefix) AND key > after ORDER BY key LIMIT limit+1`; `next` = last returned
key when more exist. `prefixEnd` increments the last code point `< 0x10FFFF` (`:85-90`).

Known keys:

| Key | Value | Writer |
|---|---|---|
| `migration.v1-v2` | `{"phase":"sessions"[,"cursor"]}` / `{"phase":"completed"}` | v1 import |
| `wellknown:sources` | `string[]` origins | `core/src/wellknown.ts:105` |
| `wellknown:manifest:<origin>` | manifest JSON | `wellknown.ts:107` |
| `wellknown:config:<origin>` | config JSON | `wellknown.ts:108` |
| `websearch:provider` | selection | `core/src/websearch.ts:31` |
| `models-dev:catalog` / `models-dev:catalog:<sha1(source)>` | `{"updatedAt":ms,"digest":sha256,"body":string}` (TTL 5 min) | `core/src/models-dev.ts:299-302,370` |
| `repository-cache:<localPath>` | `{"attemptedAt":ms}` | `core/src/repository-cache.ts:132,163` |
| `job.background/<notificationID>` | `{"id","notificationID","recovery","status","output"?,"error"?}` | `core/src/job.ts:37,200` |
| `plugin:<hex16(pluginID)>:<key>` | plugin storage; namespace = each UTF-16 unit as 4-hex-digit lowercase | `core/src/plugin/host.ts:609-617` |

### 4.12 v1 legacy model (tables `session`, `message`, `part`, `todo`) — read-only for import

`schema/src/v1/session.ts`. `message.data` = message JSON **without** `id`/`sessionID`; `part.data` = part
JSON without `id`/`messageID`/`sessionID` (the importer re-adds them, `v1-migration.bun.ts:228,252`).

- Message `role:"user"`: `time{created}`, `format?`, `summary?{title?,body?,diffs}`, `agent`, `model{providerID,modelID,variant?}`, `system?`, `tools?`.
- Message `role:"assistant"`: `time{created,completed?}`, `error?{name,data}`, `parentID`, `modelID`, `providerID`, `mode`, `agent`, `path{cwd,root}`, `summary?: bool`, `cost`, `tokens{total?,input,output,reasoning,cache{read,write}}`, `structured?`, `variant?`, `finish?`.
- Part `type` ∈ `text{text,synthetic?,ignored?,time?{start,end?},metadata?}`, `subtask{prompt,description,agent,model?{providerID,modelID},command?}`,
  `reasoning{text,metadata?,time{start,end?}}`, `file{mime,filename?,url,source?}`, `tool{callID,tool,state,metadata?}`
  (state `pending{input,raw}|running{input,title?,metadata?,time{start}}|completed{input,output,title,metadata,time{start,end,compacted?},attachments?}|error{input,error,metadata?,time{start,end}}`),
  `step-start{snapshot?}`, `step-finish{reason,snapshot?,cost,tokens}`, `snapshot{snapshot}`, `patch{hash,files}`,
  `agent{name,source?}`, `retry{attempt,error:APIError,time{created}}`, `compaction{auto,overflow?,tail_start_id?}`.
- `todo` table: v1 only; no v2 equivalent (v1 `todowrite` tool calls are dropped on import).

---

## 5. Event bus

### 5.1 Envelope (in-memory / wire `Event.Payload`)

`schema/src/event.ts:90-150`:
```json
{
  "id": "evt_…",
  "created": 1760000000000,
  "metadata": { },
  "type": "session.renamed",
  "durable": { "aggregateID": "ses_…", "seq": 7, "version": 1 },
  "location": { "directory": "/abs", "workspaceID": "wrk_…" },
  "data": { "sessionID": "ses_…", "title": "New title" }
}
```
`metadata?`, `location?` optional; `durable` present only for durable definitions. `location` is taken from
`options.location`, else the ambient `Location` service, omitted if `options.global` (`core/src/bus.ts:507-529`).

### 5.2 Durable vs ephemeral, versioning

- `Event.durable({type, durable:{aggregate:"<data field>", version:n}, schema})`; the aggregate id is
  `data[aggregate]` (`"sessionID"` for session events, `"projectID"` for `worktree.resolved`).
- Stored type string = `versionedType(type, version)` = `` `${type}.${version}` `` (`schema/src/event.ts:174-176`),
  e.g. `"session.created.1"`, `"session.deleted.2"`.
- Durable manifest (`schema/src/durable-event-manifest.ts:7`) = all durable session events (incl. internal
  `session.usage.recorded.1`, `session.message.content.updated.1`) + `worktree.resolved.1`. Unknown types in
  the `event` table are skipped on read (cursor still advances), and fail on `replay`.
- Ephemeral events are never written to SQLite.

### 5.3 Durable publish algorithm (`commitDurableEvent`, `core/src/bus.ts:284-448`)

Per aggregate id, under an in-process keyed mutex, inside an **uninterruptible `BEGIN IMMEDIATE` transaction**:
1. `SELECT seq, owner_id FROM event_sequence WHERE aggregate_id = ?`; `latest = seq ?? -1`.
2. Encode `data` with the definition schema.
3. (replay only) owner/sequence checks (§5.5).
4. `seq = latest + 1` (first event of an aggregate has **seq 0**).
5. If `persist`: die if an `event` row with the same id exists.
6. Run all registered **projectors** for `versionedType` in registration order (§5.7), passing the event with
   `durable:{aggregateID, seq, version}`. Then the publisher's optional `commit(seq)` hook (e.g. claim/release,
   blob inserts, import rows).
7. `INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?, ?, ?) ON CONFLICT(aggregate_id) DO UPDATE SET seq = max(seq, ?)` (+ `owner_id` if replaying with owner and none stored).
8. If `persist`: `INSERT INTO event (id, aggregate_id, seq, created, type, data)` with `type = "<type>.<version>"`, `data` = encoded data JSON (**no** metadata/location stored).
9. Commit; then update routing, wake `log` followers, notify listeners/subscribers (listener failures are logged, not propagated).

**`persist` defaults to `false`** (`bus.ts:203`) and the CLI/desktop server never enables it
(`server/src/routes.ts:118`, `cli/src/server-process.ts:95-136`); only workerd sets `events: { persist: true }`
(`server/src/workerd.ts:69`). Consequence for a local DB: `event` stays **empty**; `event_sequence` holds the
latest seq per session/project aggregate; projections are the source of truth.

Any projector failure (e.g. `SessionAlreadyProjected`) aborts the whole transaction → nothing is written.

### 5.4 `publishAll` (`bus.ts:531-669`)

Batch of durable events for **one** aggregate (else die `"Published events must belong to the same aggregate"`):
one immediate transaction; seqs `latest+1 … latest+n`; projectors + commit hooks per event in order; then
`event_sequence` upsert `SET seq = finalSeq` (not max); then insert rows if persist.

### 5.5 Replay / ownership (sync)

`replay(serialized, {publish?, ownerID?, strictOwner?})` (`bus.ts:671-701`): `serialized = {id, type:"x.y.N",
created?, seq, aggregateID, data}`. If `seq <= latest`: with persist, the stored row must match exactly (id, type,
created, deep-equal data) else die `"Replay diverged at aggregate <id> sequence <seq>"`; idempotent otherwise.
If `owner_id` stored and differs from `ownerID` → silently skip (strict → die). Requires `seq === latest+1`
else `"Sequence mismatch for aggregate <id>: expected <n>, got <m>"`. `claim(aggregateID, ownerID)` sets
`owner_id`. `remove(aggregateID)` deletes the `event_sequence` row and all `event` rows.
`reserveSequence(id, seq)` upserts `seq = max(seq, ?)`.

### 5.6 Durable log read (`bus.log`, `bus.ts:828-871`)

`after` exclusive cursor (default -1). Captures `target = event_sequence.seq` (or -1), pages
`SELECT * FROM event WHERE aggregate_id=? AND seq>? AND seq<=? ORDER BY seq LIMIT 512`, then emits marker
`{"type":"log.synced","aggregateID","seq"?}` (`seq` omitted when target < 0). With `follow`, subscribes to
wakes **before** reading, then reads new ranges after each commit. With `persist=false` the log yields only the marker.

### 5.7 Live subscription & routing

`subscribe()` = all events after now; with an ambient Location only events routed to that Location or
unlocated. Session events without a location are routed to the session's current `(directory, workspaceID)`
(cached, falling back to a `session_v2` lookup); `session.moved` routes to both old and new location;
`session.forked` routes to the parent's location (`bus.ts:209-261,726-746`).

### 5.8 Durable session events (all `aggregate: "sessionID"`, version 1 unless noted)

All payloads start with `"sessionID": "ses_…"`. Source `schema/src/session-event.ts`.

| Type (stored `.vN`) | Payload fields after `sessionID` | Line |
|---|---|---|
| `session.created` | `projectID`, `location: Location.Ref`, `subpath?`, `parentID?`, `slug`, `title?`, `agent?`, `model?: Model.Ref`, `metadata?`, `permissions?: Ruleset`, `version` | 48-66 |
| `session.agent.selected` | `agent`, `previous?` | 69-77 |
| `session.model.selected` | `model`, `previous?` | 80-88 |
| `session.moved` | `location`, `projectID`, `subpath?` | 91-98 |
| `session.renamed` | `title` | 101-108 |
| `session.metadata.updated` | `metadata` | 111-118 |
| `session.permissions` | `permissions` | 121-128 |
| `session.viewed` | `idle: number` | 131-139 |
| `session.message.content.updated` (internal, replay-only) | `messageID`, `content: AssistantContentEncoded[]` | 143-152 |
| `session.usage.recorded` (internal) | `source: "title"\|"compaction"`, `cost`, `tokens` | 155-164 |
| `session.deleted` **v2** | — | 177-184 |
| `session.forked` **v2** | `parentID`, `boundary: ForkBoundary`, `instructions?: {key: hash}`, `instructionEntries?: [{key, value, removed}]` | 187-200 |
| `session.inbox.delivered` | `inboxID` | 208-212 |
| `session.inbox.enqueued` | `inboxID`, `item: {type, payload, delivery}` | 215-222 |
| `session.inbox.cancelled` | `inboxID` | 225-229 |
| `session.inbox.delivery.changed` | `inboxID`, `delivery` | 232-236 |
| `session.execution.started` | — | 240 |
| `session.execution.succeeded` | — | 243 |
| `session.execution.failed` | `error: SessionError` | 246-250 |
| `session.execution.interrupted` | `reason: "user"\|"shutdown"\|"superseded"\|"inactivity"` | 253-257 |
| `session.instructions.updated` **v2** | `delta: {key: hash\|"removed"}`, `text?` | 261-276 |
| `session.synthetic` | `text`, `description?`, `metadata?` | 279-288 |
| `session.skill.activated` | `id`, `name`, `text` | 292-301 |
| `session.shell.started` | `shell: Shell.Info` | 306-313 |
| `session.shell.ended` | `shell: Shell.Info`, `output: Shell.Output` | 316-324 |
| `session.step.started` | `assistantMessageID`, `agent`, `model`, `snapshot?`, `started: int ms` | 329-341 |
| `session.step.streamed` | `assistantMessageID` | 345-352 |
| `session.step.ended` | `assistantMessageID`, `finish`, `rawFinish?`, `providerState?`, `cost`, `tokens`, `snapshot?`, `files?` | 355-369 |
| `session.step.failed` | `assistantMessageID`, `error`, `finish?: "content-filter"`, `rawFinish?`, `providerState?`, `cost?`, `tokens?`, `snapshot?`, `files?` | 372-387 |
| `session.text.started` | `assistantMessageID`, `ordinal` | 392-400 |
| `session.text.ended` | `assistantMessageID`, `ordinal`, `text`, `state?` | 415-425 |
| `session.reasoning.started` | `assistantMessageID`, `ordinal`, `state?` | 430-439 |
| `session.reasoning.ended` | `assistantMessageID`, `ordinal`, `text`, `state?` | 454-464 |
| `session.tool.input.started` | `assistantMessageID`, `id`, `name` | 476-483 |
| `session.tool.input.ended` | `assistantMessageID`, `id`, `text` | 496-503 |
| `session.tool.called` | `assistantMessageID`, `id`, `input`, `executed`, `state?` | 507-516 |
| `session.tool.success` **v2** | `assistantMessageID`, `id`, `content` (≥1), `metadata?`, `executed`, `resultState?` | 530-543 |
| `session.tool.failed` **v2** | `assistantMessageID`, `id`, `error`, `content?`, `metadata?`, `executed`, `resultState?` | 551-565 |
| `session.retry.scheduled` | `assistantMessageID`, `attempt`, `at: ms`, `error` | 569-579 |
| `session.compaction.started` | `reason`, `recent`, `inputID?` | 583-591 |
| `session.compaction.ended` | `reason`, `model?`, `providerState?`, `providerContext?`, `text`, `recent`, `cost?`, `tokens?` | 604-620 |
| `session.compaction.failed` | `reason`, `error`, `inputID?`, `cost?`, `tokens?` | 623-634 |
| `session.revert.staged` | `revert: Session.Revert` | 639-643 |
| `session.revert.cleared` | — | 644 |
| `session.revert.committed` | `to: msg id` | 645-649 |

Ephemeral session events: `session.usage.updated {sessionID, cost, tokens}`, `session.text.delta`,
`session.reasoning.delta {sessionID, assistantMessageID, ordinal, delta}`, `session.tool.input.delta {…, id, delta}`,
`session.tool.progress {…, id, metadata}`, `session.compaction.delta {sessionID, text}`.

Other durable event: `worktree.resolved` v1, aggregate `projectID`: `{projectID, directory, previous, adopted?: ProjectID[]}` (`schema/src/worktree.ts:60-68`).

### 5.9 Projection effects (`core/src/session/projector.ts`, all inside the event transaction)

| Event | Effect on tables |
|---|---|
| `session.created` | `INSERT INTO session_v2 (id, project_id, workspace_id, parent_id, slug, directory, path, title, agent, model, metadata, permission, version, time_created=created, time_updated=created) ON CONFLICT DO NOTHING RETURNING id`; no row → die `SessionAlreadyProjected` (`:447-474`) |
| `session.moved` | append `location-switched` message (previous = current row location); update `directory, path, project_id, workspace_id, time_updated=created` |
| `worktree.resolved` | re-own sessions (`workspace_id IS NULL`, project in {previous, "global", adopted…} minus new) whose directory is under the resolved directory: set `project_id`, `path = relative`, `time_updated` unchanged (`:495-551`) |
| `session.deleted` | `DELETE FROM session_v2 WHERE id=?` (cascades messages, inbox, instruction_*) |
| `session.agent.selected` / `model.selected` | append `agent-switched` / `model-switched` (previous from event or current row); set `agent`/`model`, `time_updated=created` |
| `session.renamed` / `metadata.updated` / `permissions` | set `title` / `metadata` / `permission`, `time_updated=created` |
| `session.viewed` | `time_viewed = max(idle, coalesce(time_viewed, idle))`, `time_updated` unchanged |
| `session.usage.recorded` | add cost/tokens; `time_updated` unchanged |
| `session.step.ended` | message update + add cost/tokens, `time_updated=created` |
| `session.step.failed` | message update; add usage if both cost & tokens present (`time_updated=created`), else `time_updated=created` |
| `session.step.started` | message update/append, then `time_updated=created` |
| `session.forked` | §7.2 |
| `session.inbox.enqueued` | insert `session_inbox` (die `LifecycleConflict` if a message or inbox row with that id exists); `time_updated=created` |
| `session.inbox.delivered` | delete inbox row (must exist); for `user`/`synthetic` insert a `user`/`synthetic` message with the **same id**, `time.created = event.created`, seq = event seq |
| `session.inbox.cancelled` | delete row where delivery ∈ {queue, steer} |
| `session.inbox.delivery.changed` | `UPDATE … SET delivery=to WHERE delivery=from` (from = opposite of to) |
| `session.execution.succeeded/failed/interrupted` | clear `retry` on current assistant; append `idle` message (id from event); except `interrupted{reason:"shutdown"}` (no idle message, no row update); set `time_idle = max(created, coalesce(time_idle + 1, created))`, `idle_outcome`, `time_updated` unchanged |
| `session.instructions.updated` | if `text` present append `system` message (`description: "Instructions updated: <keys joined ', '>"`, `metadata: {...event.metadata, notice:"instructions", instructionSources:[keys]}`); then `InstructionState.apply` |
| `session.compaction.ended` | message update/append, then `InstructionState.advanceEpoch(seq)` |
| `session.revert.staged` | `revert = JSON(revert)`, `time_updated=created` |
| `session.revert.cleared` | `revert = NULL`, `time_updated=created` |
| `session.revert.committed` | find boundary seq of message `to` (die if missing); delete `session_message` with `seq >= boundary`; delete `session_inbox` with `enqueued_seq >= boundary`; `revert=NULL`, `time_updated=created`; delete `instruction_state` |
| `session.execution.started` | none (publisher's commit hook sets `time_suspended`, §7.7) |

Message-level rules (`core/src/session/message-updater.ts`): new messages use id
`SessionMessage.ID.fromEvent(event.id)` (except assistant = `assistantMessageID`, compaction = `inputID ??`
fromEvent) and `time.created = event.created` (assistant uses `data.started`). Events addressing an
assistant are applied only if that assistant row exists in this session. Key behaviors:
- `step.started`: if the assistant exists, reset it (agent/model, clear retry/error/finish/rawFinish/providerState/streamed/completed, `time.created = started`, set `snapshot.start`); else mark the latest incomplete assistant `time.completed = created` and append a new one with empty `content`.
- `text.started` pushes `{"type":"text","text":""}`; `text.ended` sets text/state on the **last** text item.
- `reasoning.started` pushes `{"type":"reasoning","text":"","state","time":{"created"}}`; `reasoning.ended` updates the last uncompleted reasoning item (`time.completed = created`).
- `tool.input.started` pushes a tool item `state:{status:"streaming",input:""}`; `input.ended` sets raw input; `tool.called` → `running` with `metadata:{}`, `executed`, `providerState`, `time.ran`; `tool.success` (only from `running`) → `completed`; `tool.failed` (from streaming/running) → `error` with `input = {}` if it was a string. `executed = event.executed || previous === true`.
- `step.ended`: `time.completed`, `finish`, `rawFinish`, `providerState`, `cost`, `tokens`, and `snapshot = {...snapshot, end, files}` when either given. `step.failed`: same with `finish ?? "error"`, `error`, `retry` cleared, usage only if both present.
- `retry.scheduled`: `retry = {attempt, at, error}`.
- `shell.started` appends `shell` message (`metadata` gets `background: true` if `shell.metadata.background === true`); `shell.ended` updates latest shell message with that `shellID`.
- `compaction.started` appends running compaction (`summary:""`); `ended` completes the running one (merging metadata) or appends completed; `failed` converts running or appends failed.
- `session.synthetic` / `skill.activated` append `synthetic` / `skill` messages.

Message updates rewrite `type`, `time_created` (from `message.time.created`), `data`; `time_updated` = now.
After commit, `session.step.ended`, `session.step.failed` (with usage) and `session.usage.recorded` trigger an
ephemeral `session.usage.updated` with the row totals (`projector.ts:796-806`).

### 5.10 Other (ephemeral) event types

`project.updated` (data = `Project.Info` fields), `worktree.updated {projectID}`, `worktree.ready {name, branch?}`,
`worktree.failed {message}`, `workspace.ready {name}`, `workspace.failed {message}`,
`workspace.status {workspaceID, status: connected|connecting|disconnected|error}`,
`credential.updated {}`, `credential.switched {integrationID, credentialID: string|null}` (published `global`),
`permission.asked` (Request fields), `permission.replied {sessionID, requestID, reply}`,
`form.created {form}`, `form.replied {id, sessionID, answer}`, `form.cancelled {id, sessionID}`,
`shell.created {info}`, `shell.exited {id, exit?, status}`, `shell.deleted {id}`,
`pty.created|pty.updated {info}`, `pty.exited {id, exitCode}`, `pty.deleted {id}`,
`persistent-pty.added {sessionID, terminal}`, `persistent-pty.removed {sessionID, ptyID}`,
`filesystem.changed {file, event: add|change|unlink}`, `file.edited {file}` (v1),
`vcs.branch.updated {branch?}`, `session.status {sessionID, status: {type:"idle"}|{type:"busy"}|{type:"retry",attempt,message,action?,next}}`,
`session.idle {sessionID}` (deprecated), `session.compacted {sessionID}`,
`installation.updated|installation.update-available {version}`, `lsp.updated`, `mcp.tools.changed|mcp.resources.changed|mcp.status.changed {server}`,
`tui.prompt.append {text}`, `tui.command.execute {command}`, `tui.toast.show {title?, message, variant, duration=5000}`,
`tui.session.select {sessionID}`, `command.executed {name, sessionID, arguments, messageID}` (v1),
`location.shutdown`, `server.connected`, `global.disposed`, and empty-payload `*.updated` for
`skill, plugin, reference, provider, model, agent, command, config, integration, websearch`, `models-dev.refreshed`.
Manifests: `schema/src/event-manifest.ts`.

---

## 6. Snapshots, retention, repository cache, State

### 6.1 Git snapshots (`core/src/snapshot.ts`, `core/src/git.ts`)

- Enabled only when the Location's VCS is git and state `enabled` (default true; configurable via State editor `configure(enabled)`).
- **Store location**: `<data>/snapshot/<projectID>/<sha1(realpath(source worktree))>` (`snapshot.ts:95-97`) — a bare-ish
  git dir used with `--git-dir <store> --work-tree <source worktree>`; every git call is
  `git --git-dir <gitDirectory> --work-tree <worktree> <args>` with `cwd = worktree` (`git.ts:326-332`).
- **Create** (`git.ts:375-430`): init in `<store>.init-<pid>-<rand>` then rename into place; `git init`;
  write `opencode.gitconfig`:
  ```
  [core]
  	autocrlf = false
  	longpaths = true
  	symlinks = true
  	fsmonitor = false
  	untrackedCache = true
  	# A split index cannot name its shared file once manyFiles skips index checksums.
  	splitIndex = false
  [feature]
  	manyFiles = true
  [index]
  	version = 4
  	threads = true
  ```
  and append `[include]\n\tpath = opencode.gitconfig\n` to `config`; write `objects/info/alternates` =
  `<source common dir>/objects\n`; copy source `index` (best-effort).
- **Capture** (returns tree SHA or `undefined` on failure): scope = location dir relative to worktree (or `.`);
  hard-link (fallback copy, preserving mtime) the store index to `index.opencode-<ms>-<pid>-<rand>`; with
  `GIT_INDEX_FILE` = temp: `diff-files --name-only -z -- :(literal)<scope>` and
  `ls-files --others --exclude-standard -z -- :(literal)<scope>`; drop paths ignored by the **source** repo
  (`check-ignore --no-index --stdin -z` against the source); skip untracked files > 2 MiB;
  `update-index --force-remove -z --stdin` for now-ignored tracked paths; `update-index --add --remove --replace -z --stdin`;
  `write-tree`; rename temp index over `index`. Fast path returns last tree if nothing changed. Unreadable
  index → rebuild from source index. Stale temp indexes (>1 h) swept.
- **Pack** every ≥10 min when ≥2048 loose objects or ≥16 packs: `pack-objects -q --non-empty` with explicit
  object list (never prunes), lock file `opencode-pack.lock` (stale after 1 h).
- **files(from,to)**: `diff --name-only --no-renames -z <from> <to>`, minus source-ignored paths.
- **diff(from,to,context=3,paths?)**: `--literal-pathspecs diff --name-status -z`, `--numstat -z`, and
  `-c core.quotepath=false diff --no-ext-diff --unified=<n>` → `FileDiff.Info[]` (binary → patch `""`).
- **restore(files: path→tree)**: `git checkout <tree> --pathspec-from-file=- --pathspec-file-nul` (batched per
  tree) using a temp index; paths absent in the tree are deleted from the worktree.

### 6.2 File retention (`core/src/file-retention.ts`)

Deletes listed files whose mtime < now − retention. Used hourly for:
`<data>/tool-output/tool_<12hex>…` (7 days, `core/src/tool-output.ts:13-16,37-47`) and
`<data>/shell/<projectID>/sh_<12hex>….out` (7 days, `core/src/shell.ts:33-34,86-106`).

### 6.3 Repository cache (`core/src/repository-cache.ts`)

Clones remote repos to `<data>/repos/<host parts split on ':'>/<path segments>[@<encodeURIComponent(branch)>]`
under a file lock keyed `repository-cache:<localPath>`; refresh ("daily" = ≥24 h since KV `attemptedAt`, or
forced) = fetch, optional fetch branch, checkout remote branch, `reset --hard origin/<branch>`. The attempt
time is written to KV **before** network work.

### 6.4 State (`core/src/state.ts`)

Purely in-memory registry of replayable config/plugin "transforms" (e.g. snapshot enabled flag, tool-output
limits). Nothing persisted. Session environment variables are also in-memory only
(`core/src/session/environment.ts`).

---

## 7. Session store operations

### 7.1 Create (`core/src/session.ts:262-318`)

1. `id = input.id ?? ses_<descending>`; if it exists return it (idempotent).
2. `parentID` given → parent must exist (else `NotFoundError`); location = parent location ?? input location.
3. `project = Project.resolve(location.directory)` (upserts `project`, may emit `worktree.resolved`).
4. Publish `session.created` with `slug`, `version = app.version`, `projectID`, `parentID`, `location`,
   `subpath = relative(project.directory, location.directory)` with `\`→`/`, `title`, `agent`,
   `metadata ?? parent.metadata`, `permissions ?? parent.permissions`, `model`; envelope `location`.
5. On `SessionAlreadyProjected` return the existing row.

### 7.2 Fork (`session.ts:319-359`, projector `projectFork` `projector.ts:115-235`)

- Boundary = message `before` (type `"before"`) or the latest message (type `"through"`); no messages →
  `ForkEmptyError`; unknown `before` → `MessageNotFoundError`.
- Event `session.forked` (v2) on the **new** session aggregate, carrying parent's current
  `instruction_state.current_values` and the parent's `instruction_entry` snapshot (ordered by key).
- Projection: insert `session_v2` (`parent_id NULL`, `fork_session_id = parent`, `fork_boundary`, copy
  project/workspace/directory/path/agent/model/metadata/permission/version, new slug, title
  `forkTitle(parent.title)` = `"<t> (fork #1)"` or increments `" (fork #N)"`, zero usage, times = created).
  Copy instruction entries (time = created). Copy parent messages with `seq < boundary` (`before`) or `<=`
  (`through`) in batches of 500, **excluding** unsettled ones (assistant without `$.time.completed`, shell/compaction
  with `$.status = 'running'`), new id `msg_<forkEventSuffix>_<seq>`, same seq/time_created/time_updated/data.
  `reserveSequence(child, copiedSeq)` so the next child event gets `copiedSeq + 1` (the fork event itself is seq 0).
  `InstructionState.initialize(child, 0, instructions)`.

### 7.3 Delete (`session.ts:367-377`)

Interrupt and await idle, close transport, recursively delete children (`parent_id = id`, sequentially),
clear env, publish `session.deleted` (projector deletes the row → FK cascades), then `bus.remove(id)`
(deletes `event_sequence` + `event` rows for the aggregate).

### 7.4 Revert (`core/src/session/revert.ts`)

- `stage(messageID, files?)`: busy → `BusyError`. `original = session.revert.snapshot ?? snapshot.capture()`.
  Plan = for assistant messages with `seq > boundary.seq` having `snapshot.start`, map each `snapshot.files`
  path → that start tree (first wins). Restore previous revert files to `original`, plus planned files (unless
  `files === false`). `files = diff(original → fresh capture, paths)`. Publish `session.revert.staged
  {revert:{messageID, snapshot: original, files}}`.
- `clear`: restore revert files to `original`; publish `session.revert.cleared`.
- `commit`: publish `session.revert.committed {to: revert.messageID}` (projection deletes messages with
  `seq >= boundary` — note: inclusive, while staging planned `seq > boundary`). Committed implicitly before a
  new prompt or compaction is admitted.

### 7.5 Move (`core/src/session/move.ts`)

Resolve `~`/relative destination against the session dir; must exist and be a directory; resolve project;
probe destination Location. If the source is unavailable and the session is idle, publish (atomically via
`publishAll`) `session.inbox.cancelled` for pending move items + `session.moved {location, projectID, subpath}`;
otherwise admit an inbox `move` item (`delivery` default `"steer"`) and wake execution.

### 7.6 Other mutations

`rename` → `session.renamed`; `setMetadata` → `session.metadata.updated`; `setPermissions` → `session.permissions`;
`switchAgent` → `session.agent.selected {agent, previous}`; `switchModel` → `session.model.selected` (no-op if
same provider/id/variant with `variant ?? "default"`); `view(idle)` → `session.viewed` only if
`time.idle` exists, `idle <= time.idle`, and not already viewed at ≥ idle (`core/src/session/session.ts:61-70`).
Prompt/synthetic/compact admit inbox items (`msg_` ascending id, default delivery `"steer"`).

### 7.7 Execution claim

`session.execution.started` commit hook: `UPDATE session_v2 SET time_suspended = now, time_updated = time_updated WHERE id=? AND time_suspended IS NULL`;
terminal events: `time_suspended = NULL, resume_attempts = 0`; `countResume` increments `resume_attempts`;
`listSuspended` = rows with `time_suspended IS NOT NULL AND parent_id IS NULL` (`core/src/session/store.ts:194-251`,
`core/src/session/execution.ts:76-86`).

### 7.8 Listing / pagination (`core/src/session/store.ts`)

- Sessions: filters `directory =`, `workspace_id =`, `project_id =` (+ `path =`), `title LIKE '%search%'`,
  `parent_id IS NULL` / `=`; order `time_updated` then `id` (default DESC); anchor `{id, time, direction}` keyset;
  `previous` direction reverses order and result.
- Messages: ordered by `seq` (default DESC), optional `type`, cursor by message id → its seq (exclusive).
- Context (`SessionHistory.load`): messages from the latest completed compaction (`json_extract(data,'$.status')='completed'`)
  onward, ordered by seq, with system messages older than the last native checkpoint dropped.

### 7.9 Export / import (`core/src/session/transfer.ts`)

**Format** `SessionTransfer.Data` (`schema/src/session-transfer.ts`):
```json
{
  "info": { …Session.Info (§4.2)… },
  "messages": [ { "id": "msg_…", "metadata"?: {}, "time": { "created": 1760000000000 }, "type": "user", "text": "…" }, … ]
}
```
Messages are full `SessionMessage.Info` (with `id` and `type`), ascending by seq, **settled only**
(assistant with `time.completed`; shell/compaction `status !== "running"`) (`transfer.ts:165-169`). The CLI
writes `JSON.stringify(data, null, 2) + EOL` to stdout (`cli/src/commands/handlers/session/export.ts`).

`sanitize=true` (`transfer.ts:171-336`) replaces non-blank strings with `"[redacted:<kind>:<id>]"` (title →
`session-title`, directory → `"/[redacted:session-directory:<id>]"`, message text, files (data `""`, source
inline), tool inputs `{"redacted":"tool-input:<id>"}`, metadata → `{"redacted":"<kind>:<id>"}` when non-empty, etc.).

**Import**: conflict if the session id exists (`ImportConflictError`, HTTP 409). Parent must exist. Resolve
project for the target location and upsert it. Publish `session.created` (new slug, current app version,
target location, original title/agent/model/metadata/permissions/parentID) with a commit hook that inserts
the settled messages with `seq = 1..n` (row `time_created = message.time.created`), `reserveSequence(seq + n)`,
and updates `cost`, tokens, `time_created` (original), `time_updated = import time`, `time_idle`,
`time_viewed = min(idle, viewed)` only if both, `idle_outcome` only if idle, `time_archived`. HTTP:
`POST /api/experimental/session/import` with body `{...data, "location": {"directory"}}`.

---

## 8. Open questions

1. **JSON key order**: the claim that Effect `Schema.encode` emits struct keys in declaration order (and the
   resulting `data` byte layout) was inferred from source, not executed (no `node_modules` available). Verify
   by running the TS encoder once and diffing against the Rust output.
2. **Drizzle `$onUpdate` in `onConflictDoUpdate`**: whether upserts (e.g. `upsertProject` setting only `vcs`,
   `reserveSequence`) bump `time_updated` implicitly is Drizzle-internal behavior; confirm against a live DB.
3. **Migration 12 multi-statement `tx.run`** (`ALTER …; UPDATE …;` in one string): with `bun:sqlite`
   `query().all()` only the first statement may execute, leaving `icon_url_override` unbackfilled on old DBs.
   Decide whether the Rust port reproduces this.
4. **Events table locally empty**: with `persist=false` (CLI default) `Session.log` replays nothing. Confirm the
   Rust port should also not write `event` rows locally (byte-compat suggests yes).
5. **ID wraparound** (§3.1): IDs only encode `ts mod 2^36` ms; sorting by id is not chronological across the
   ~2.18-year wrap. Replicate as-is (assumed) — confirm intent.
6. **`session_pending`, `account*`, `control_account`, `project_directory`, `summary_*`, `share_url`,
   `time_compacting`**: present but unused by v2 code; the port should create/keep them but need not read them.
7. Workerd/Durable Object variant (no tuning pragmas, `defer_foreign_keys`) is out of scope here.
8. Revert staging plans files from messages with `seq > boundary` while commit deletes `seq >= boundary`; the
   boundary message itself (normally a user message) is removed on commit. Confirm this asymmetry is intended.

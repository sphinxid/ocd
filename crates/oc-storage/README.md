# oc-storage

SQLite storage shared with upstream opencode (`core/src/database`, `core/src/session`): the same database file,
schema and migrations, the durable session event commit and its projection into the session tables, and the reads
that turn stored rows back into messages. ocd and upstream 2.3.1 open each other's databases.

## Modules

| module | contents |
|---|---|
| `db` | `Database`: one connection behind a mutex, `with`, `immediate` (one `BEGIN IMMEDIATE` transaction), `memory()` for tests |
| `migrate` | Bootstrap of a fresh database and upgrades of older ones with upstream's migration ids, the legacy credential import |
| `failure` | SQLite result codes that mean storage itself can't be used, with upstream's messages |
| `kv` | The `kv` table |
| `projector` | `Database::commit_session_events`: sequence numbers, the projection of every durable session event (`session/projector.ts`) and the message updater (`message-updater.ts`) |
| `session` | Session rows and message reads |
| `stored` | Stored message shapes: `session_part` rows (`withParts`, `writeParts`), media in `blob` behind `media:` refs (externalize, hydrate, keep refs, release) |

## Session projection

`Committing` carries one durable event and the config it projects under, which `oc_core::bus::Bus` resolves from the
session's location:

- `media`: `experimental.media_blob`. On, inline images in user files and tool results go to `blob` and rows hold
  `media:<sha256>` refs; `blob_ref` keeps each blob alive while a message names it.
- `part_rows`: `experimental.part_rows` (on by default since 2.4.1, also for events published outside any location;
  `false` turns it off). Off, every
  assistant event rewrites the whole message: inline in its row, or into the `session_part` rows the message already
  has (written with the flag on, by a fork, or by upstream), touching only the rows that changed. On, the part events
  write single rows: `session.text.started`, `reasoning.started` and `tool.input.started` append a row after the
  message's last ordinal, the `*.ended`, `tool.called`, `tool.success` and `tool.failed` events update the last
  matching row (by type, call ID, or incomplete reasoning) only when it changes, and `session.message.content.updated`
  replaces them all from ordinal 0. Envelope updates (step, retry, shell and compaction events) leave the row's
  `content` as stored. A message an older build wrote inline moves to part rows on its first part write, its inline
  parts appended after any rows it already has.

Forks copy part rows under the forked message IDs, reverts and session deletes remove them with their messages, and
every read folds them back into `content` (part rows first, then any inline content), so a database reads the same
whatever the flag was when each event was written.

## Tests

`tests/projector.rs` (event projection), `tests/stored.rs` (migrations and schema against upstream 2.4.1's, part rows a
newer release wrote, media blobs), `tests/native.rs` (pre-v2.1 native data), `tests/part_rows.rs` (the part-row writer:
upstream's `session-projector.test.ts` and `session-store.test.ts` cases with `part_rows: true`, envelope updates,
forks, reverts and deletes, the flag switched mid-session).

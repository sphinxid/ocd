#!/bin/sh
# why-stopped.sh [SESSION_ID] [DB]: read-only look at why an ocd/opencode session's run ended.
# Without SESSION_ID: the 10 most recently updated root sessions. DB defaults to the release-channel database.
DB=${2:-${XDG_DATA_HOME:-$HOME/.local/share}/opencode/opencode.db}
URI="file:$DB?mode=ro"
if [ -z "$1" ]; then
  sqlite3 -header -column "$URI" "
SELECT id, substr(coalesce(title, ''), 1, 40) AS title, idle_outcome AS last_outcome,
       datetime(time_updated / 1000, 'unixepoch', 'localtime') AS updated,
       time_suspended IS NOT NULL AS claimed, resume_attempts
FROM session_v2 WHERE parent_id IS NULL ORDER BY time_updated DESC LIMIT 10;"
  exit
fi
sqlite3 -header -column "$URI" "
SELECT seq, strftime('%Y-%m-%d %H:%M:%S', time_created / 1000, 'unixepoch', 'localtime') AS at, type,
  CASE type
    WHEN 'assistant' THEN
      'finish=' || coalesce(json_extract(data, '$.finish'), '-') || ' (raw ' || coalesce(json_extract(data, '$.rawFinish'), '-') || ')'
      || ' tools=' || (SELECT count(*) FROM json_each(data, '$.content') WHERE json_extract(value, '$.type') = 'tool')
      || coalesce(' tool-errors=' || (SELECT group_concat(json_extract(value, '$.name') || ':' || json_extract(value, '$.state.error.type'), ',')
                                       FROM json_each(data, '$.content') WHERE json_extract(value, '$.state.status') = 'error'), '')
      || coalesce(' text=\"' || substr(replace((SELECT group_concat(json_extract(value, '$.text'), ' ') FROM json_each(data, '$.content')
                                                WHERE json_extract(value, '$.type') = 'text'), char(10), ' '), -80) || '\"', '')
      || coalesce(' STEP ERROR ' || json_extract(data, '$.error.type') || ': ' || json_extract(data, '$.error.message'), '')
    WHEN 'idle' THEN
      '== RUN ENDED: ' || json_extract(data, '$.outcome')
      || coalesce(' ' || json_extract(data, '$.error.type') || ': ' || json_extract(data, '$.error.message'), '')
    WHEN 'compaction' THEN
      coalesce(json_extract(data, '$.reason'), '') || ' compaction ' || json_extract(data, '$.status')
      || coalesce(': ' || json_extract(data, '$.error.type') || ': ' || json_extract(data, '$.error.message'), '')
    ELSE substr(replace(coalesce(json_extract(data, '$.text'), data), char(10), ' '), 1, 110)
  END AS detail
FROM session_message WHERE session_id = '$1' ORDER BY seq DESC LIMIT ${LIMIT:-30};"
sqlite3 -header -column "$URI" "
SELECT idle_outcome AS last_outcome, time_suspended IS NOT NULL AS claimed, resume_attempts,
       (SELECT count(*) FROM session_inbox WHERE session_id = '$1') AS inbox_waiting
FROM session_v2 WHERE id = '$1';"

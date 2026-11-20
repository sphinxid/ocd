"""The opt-in `experimental.part_rows` writer (2.2.7, unchanged in 2.3.1): each assistant part is its own
`session_part` row and the message row keeps an empty `content` envelope. The scenarios compare the stored rows
(`session_message` and `session_part`), not only what the API reads back: a loop with reasoning, parallel tools and
text, forks and a revert, a compaction, a session moved between a location with the flag and one without, and
databases written by one binary and read (and written on) by the other with the flag on and off."""
import json
import os
import sqlite3
import time

from parity.artifact import database
from parity.media import ints
from parity.registry import scenario

ON = {"experimental": {"part_rows": True}}


def db_path(ctx):
    return ctx.env_extra.get("OPENCODE_DB") or database(ctx)


def stored(ctx, label, sid):
    """The session's rows as stored: messages in `seq` order, then part rows by message and ordinal. Each part row also
    lists its data's key order; timestamps are masked by the normalizer, so only whether a row was touched after it was
    created is kept."""
    con = sqlite3.connect(db_path(ctx))
    try:
        m = con.execute("SELECT id, type, seq, data FROM session_message WHERE session_id = ? ORDER BY seq", (sid,)).fetchall()
        p = con.execute(
            "SELECT p.message_id, p.ordinal, p.type, p.call_id, p.time_updated > p.time_created, p.data FROM session_part AS p "
            "JOIN session_message AS m ON m.id = p.message_id WHERE p.session_id = ? ORDER BY m.seq, p.ordinal", (sid,)).fetchall()
    finally:
        con.close()
    ctx.rec(label, {
        "message": [[i, t, s, ints(json.loads(d))] for i, t, s, d in m],
        "part": [[i, o, t, c, bool(u), list(json.loads(d).keys()), ints(json.loads(d))] for i, o, t, c, u, d in p],
    })


def counts(ctx, label):
    con = sqlite3.connect(db_path(ctx))
    try:
        ctx.rec(label, {t: con.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
                        for t in ("session_v2", "session_message", "session_part")})
    finally:
        con.close()


def reads(ctx, label, sid):
    ctx.call(f"{label}.messages", "GET", f"/api/session/{sid}/message?order=asc")
    ctx.call(f"{label}.context", "GET", f"/api/session/{sid}/context")


def users(ctx, sid):
    _, msgs = ctx.call("find", "GET", f"/api/session/{sid}/message?order=asc", record=False)
    return [m["id"] for m in (msgs or {}).get("data", []) if m.get("type") == "user"]


TOOLS = [
    {"reasoning": "Look at both files.", "text": "Reading two files.",
     "tools": [{"name": "read", "id": "call_a", "args": {"path": "$PROJECT/a.txt"}},
               {"name": "glob", "id": "call_b", "args": {"pattern": "*.txt"}}]},
    {"reasoning": "Both read.", "text": "a.txt says alpha."},
]


def setup(ctx, script, config=ON, **project):
    ctx.set_script(script)
    files = {"a.txt": "alpha\n", "b.txt": "beta\n", **project.pop("files", {})}
    ctx.make_project(files=files, config_extra=config, **project)
    ctx.start()


@scenario("partrows")
def partrows_loop(ctx):
    """part_rows on: a step with reasoning, text and two parallel tools, then a step with reasoning and text; a failed
    tool call and a retried step; the rows, the reads and the next request's replay."""
    setup(ctx, TOOLS + [
        {"tools": [{"name": "read", "id": "call_missing", "args": {"path": "$PROJECT/missing.txt"}}]},
        {"status": 500, "body": {"error": {"message": "boom"}}},
        {"text": "Done after the retry."},
    ])
    sid = ctx.create_session()
    ctx.prompt(sid, "read the files")
    ctx.prompt(sid, "read a missing file", timeout=90)
    stored(ctx, "rows", sid)
    reads(ctx, "read", sid)
    ctx.record_llm()


@scenario("partrows")
def partrows_fork_revert(ctx):
    """part_rows on: two prompts, a fork of everything and one before the second prompt (part rows copied under the
    forked messages), a revert committed at the second prompt (its rows deleted), the session deleted."""
    setup(ctx, TOOLS + [{"text": "Second answer."}, {"text": "Fork answer."}], git_repo=True)
    sid = ctx.create_session()
    ctx.prompt(sid, "read the files")
    ctx.prompt(sid, "two")
    u = users(ctx, sid)
    f1 = ctx.data("fork.all", "POST", f"/api/session/{sid}/fork", {})["id"]
    stored(ctx, "rows.fork.all", f1)
    reads(ctx, "read.fork.all", f1)
    f2 = ctx.data("fork.before", "POST", f"/api/session/{sid}/fork", {"before": u[1]})["id"]
    stored(ctx, "rows.fork.before", f2)
    ctx.prompt(f1, "in the fork")
    stored(ctx, "rows.fork.prompted", f1)
    ctx.call("revert.stage", "POST", f"/api/session/{sid}/revert/stage", {"messageID": u[1]})
    ctx.call("revert.commit", "POST", f"/api/session/{sid}/revert/commit")
    stored(ctx, "rows.reverted", sid)
    reads(ctx, "read.reverted", sid)
    counts(ctx, "counts")
    ctx.call("delete", "DELETE", f"/api/session/{sid}")
    counts(ctx, "counts.deleted")
    reads(ctx, "read.fork.after.delete", f1)


@scenario("partrows", hold_for_title=0)
def partrows_compaction(ctx):
    """part_rows on: a manual compaction after a tool step (the summary request replays the part rows, the compaction
    row is written whole), then a prompt after it."""
    setup(ctx, TOOLS + [{"text": "Summary of the conversation."}, {"text": "After compaction."}])
    sid = ctx.create_session(title="Fixed")
    ctx.prompt(sid, "read the files")
    mark = len(ctx.server.events)
    ctx.call("compact", "POST", f"/api/session/{sid}/compact", {})
    ctx.wait_idle(sid, start=mark, timeout=60)
    ctx.prompt(sid, "after")
    stored(ctx, "rows", sid)
    reads(ctx, "read", sid)
    ctx.record_llm()


@scenario("partrows")
def partrows_move(ctx):
    """a session moved between the project (part_rows off) and a directory with it on: the step there writes part
    rows, the step back in the project rewrites whole messages again (into the rows a message already has)."""
    setup(ctx, [{"text": "First, flag off."}, {"text": "Second, flag on."}, {"text": "Third, flag off."}], config={})
    other = os.path.join(ctx.work, "other")
    os.makedirs(other, exist_ok=True)
    with open(os.path.join(ctx.project, "opencode.json")) as f:
        cfg = {**json.load(f), **ON}
    with open(os.path.join(other, "opencode.json"), "w") as f:
        json.dump(cfg, f)
    sid = ctx.create_session()
    ctx.prompt(sid, "one")
    for label, directory, text in (("move.other", other, "two"), ("move.back", ctx.project, "three")):
        mark = len(ctx.server.events)
        ctx.call(label, "POST", f"/api/session/{sid}/move", {"directory": directory})
        ctx.wait_for("session.moved", start=mark, timeout=20)
        time.sleep(1)
        ctx.prompt(sid, text)
    stored(ctx, "rows", sid)
    reads(ctx, "read", sid)


def write_config(ctx, flag):
    with open(os.path.join(ctx.project, "opencode.json")) as f:
        cfg = json.load(f)
    cfg["experimental"] = {"part_rows": flag}
    with open(os.path.join(ctx.project, "opencode.json"), "w") as f:
        json.dump(cfg, f, indent=2)


def restart(ctx, binary):
    from parity.harness import Server
    ctx.stop()
    ctx.server = Server(binary, ctx.project, ctx.env(), ctx.name, None)
    ctx.server_url = ctx.server.url
    ctx.event_start = len(ctx.server.events)
    ctx._booted = False


@scenario("partrows", timeout=300)
def partrows_roundtrip(ctx):
    """one database, both binaries: this binary writes a session with part_rows on (tools, a fork, a revert); the other
    binary reads it with the flag on and off and continues it (with the flag off, then on); this binary reads the result.
    The upstream run is "upstream writes, the port reads" and the port's run the reverse, so identical dumps mean each
    reads the other's rows as its own."""
    peer = getattr(ctx, "peer", None)
    ctx.env_extra["OPENCODE_DB"] = os.path.join(ctx.state, "shared.db")
    setup(ctx, TOOLS + [{"text": "Second answer."}, {"text": "Peer, flag off."}, {"text": "Peer, flag on."},
                        {"text": "Back home."}], git_repo=True)
    sid = ctx.create_session()
    ctx.prompt(sid, "read the files")
    ctx.prompt(sid, "two")
    u = users(ctx, sid)
    fork = ctx.data("fork", "POST", f"/api/session/{sid}/fork", {})["id"]
    ctx.call("revert.stage", "POST", f"/api/session/{sid}/revert/stage", {"messageID": u[1]})
    ctx.call("revert.commit", "POST", f"/api/session/{sid}/revert/commit")
    stored(ctx, "written.rows", sid)
    reads(ctx, "written", sid)
    reads(ctx, "written.fork", fork)
    if not peer:
        ctx.rec("SKIPPED", "no peer binary")
        return
    for flag, text in ((False, "peer off"), (True, "peer on")):
        write_config(ctx, flag)
        restart(ctx, peer)
        ctx.booted()
        reads(ctx, f"peer.{'on' if flag else 'off'}", sid)
        reads(ctx, f"peer.{'on' if flag else 'off'}.fork", fork)
        ctx.prompt(sid, text)
        stored(ctx, f"peer.{'on' if flag else 'off'}.rows", sid)
    for flag in (True, False):
        write_config(ctx, flag)
        restart(ctx, ctx.binary)
        ctx.booted()
        reads(ctx, f"home.{'on' if flag else 'off'}", sid)
    ctx.prompt(sid, "home")
    stored(ctx, "home.rows", sid)
    ctx.call("delete.fork", "DELETE", f"/api/session/{fork}")
    counts(ctx, "counts")

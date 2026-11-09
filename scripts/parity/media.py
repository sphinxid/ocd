"""Stored media (`experimental.media_blob`): inline images in prompts and tool results go to the `blob` table behind
`media:<hash>` refs; reads hydrate them, or keep them for clients sending `X-OpenCode-Capabilities:
experimental-media-uri`; forks and imports store media where their location says so; revert, delete and inbox cancel
release blobs; `GET /api/experimental/media/<hash>` serves referenced ones."""
import base64
import json
import os
import sqlite3

from parity.artifact import database, raw
from parity.registry import scenario

ON = {"experimental": {"media_blob": True}}
# 1x1 PNG (passes through the image normalizer unchanged).
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
PNG_URI = "data:image/png;base64," + base64.b64encode(PNG).decode()
REFS = {"x-opencode-capabilities": "experimental-media-uri"}


def blobs(ctx, label):
    con = sqlite3.connect(database(ctx))
    try:
        b = con.execute("SELECT hash, mime, bytes, width, height, length(data) FROM blob ORDER BY hash").fetchall()
        r = con.execute("SELECT session_id, message_id, hash, seq FROM blob_ref ORDER BY session_id, seq, message_id").fetchall()
    finally:
        con.close()
    ctx.rec(label, {"blob": [list(x) for x in b], "blob_ref": [list(x) for x in r]})


def ints(v):
    """Whole floats as integers: the port stores `cost` as `0.0` in message rows where upstream writes `0` (an older,
    unrelated difference of the stored JSON text; both decode to the same number)."""
    if isinstance(v, dict):
        return {k: ints(x) for k, x in v.items()}
    if isinstance(v, list):
        return [ints(x) for x in v]
    if isinstance(v, float) and v.is_integer():
        return int(v)
    return v


def rows(ctx, label, sid):
    con = sqlite3.connect(database(ctx))
    try:
        m = con.execute("SELECT id, type, seq, data FROM session_message WHERE session_id = ? ORDER BY seq", (sid,)).fetchall()
        i = con.execute("SELECT id, type, payload FROM session_inbox WHERE session_id = ? ORDER BY enqueued_seq", (sid,)).fetchall()
    finally:
        con.close()
    ctx.rec(label, {"message": [[a, b, c, ints(json.loads(d))] for a, b, c, d in m],
                    "inbox": [[a, b, ints(json.loads(c))] for a, b, c in i]})


def reads(ctx, label, sid, message=None):
    ctx.call(f"{label}.messages", "GET", f"/api/session/{sid}/message?order=asc")
    ctx.call(f"{label}.messages.refs", "GET", f"/api/session/{sid}/message?order=asc",
             headers={"x-opencode-directory": ctx.project, **REFS})
    ctx.call(f"{label}.inbox.refs", "GET", f"/api/session/{sid}/inbox", headers={"x-opencode-directory": ctx.project, **REFS})
    if message:
        ctx.call(f"{label}.message", "GET", f"/api/session/{sid}/message/{message}")
        ctx.call(f"{label}.message.refs", "GET", f"/api/session/{sid}/message/{message}",
                 headers={"x-opencode-directory": ctx.project, "X-OpenCode-Capabilities": "other, Experimental-Media-URI"})


def fresh_ids(data):
    """An export with new session and message IDs, so it imports next to its original."""
    data["info"]["id"] = "ses_000000000000importimportb"
    for i, m in enumerate(data.get("messages", [])):
        m["id"] = f"msg_{i + 1:012x}importimportab"
    return data


def first_user(ctx, sid):
    _, data = ctx.call("find", "GET", f"/api/session/{sid}/message?order=asc", record=False)
    for m in (data or {}).get("data", []):
        if m.get("type") == "user":
            return m["id"]
    return None


def setup(ctx, config=ON, script=None):
    ctx.set_script(script or [
        {"tools": [{"name": "read", "id": "call_read", "args": {"path": "$PROJECT/shot.png"}}]},
        {"text": "A pixel."},
    ])
    ctx.make_project(config_extra=config)
    with open(os.path.join(ctx.project, "shot.png"), "wb") as f:
        f.write(PNG)
    ctx.start()


@scenario("media")
def media_blob_store_and_read(ctx):
    """media_blob on: a prompt image and a read tool's image become one blob with refs; plain reads hydrate, reads
    with the capability keep refs (with sizes), the media route serves the bytes, a pending prompt keeps its ref."""
    setup(ctx)
    sid = ctx.create_session()
    ctx.prompt(sid, "What is in [Image 1]", files=[{"uri": PNG_URI, "name": "shot.png",
                                                     "mention": {"start": 11, "end": 20, "text": "[Image 1]"}}])
    ctx.call("pending", "POST", f"/api/session/{sid}/prompt",
             {"text": "later", "files": [{"uri": PNG_URI}], "resume": False})
    blobs(ctx, "blobs")
    rows(ctx, "rows", sid)
    reads(ctx, "read", sid, first_user(ctx, sid))
    con = sqlite3.connect(database(ctx))
    try:
        found = con.execute("SELECT hash FROM blob LIMIT 1").fetchone()
    finally:
        con.close()
    if found:
        h = found[0]
        raw(ctx, "media.get", "GET", f"/api/experimental/media/{h}")
        raw(ctx, "media.get.etag", "GET", f"/api/experimental/media/{h}", headers={"if-none-match": f'W/"{h}"'})
    raw(ctx, "media.missing", "GET", "/api/experimental/media/" + "0" * 64)
    ctx.record_llm()


@scenario("media")
def media_blob_off(ctx):
    """media_blob off: media stays inline and the capability changes nothing."""
    setup(ctx, config={})
    sid = ctx.create_session()
    ctx.prompt(sid, "look", files=[{"uri": PNG_URI}])
    blobs(ctx, "blobs")
    rows(ctx, "rows", sid)
    reads(ctx, "read", sid, first_user(ctx, sid))


@scenario("media")
def media_blob_fork_import_release(ctx):
    """forks and imports store media as blobs where media_blob is on; revert, inbox cancel and session delete
    release them."""
    setup(ctx)
    sid = ctx.create_session()
    ctx.prompt(sid, "look", files=[{"uri": PNG_URI, "name": "a.png"}])
    first = first_user(ctx, sid)
    _, forked = ctx.call("fork", "POST", f"/api/session/{sid}/fork", {})
    child = (forked or {}).get("data", {}).get("id")
    blobs(ctx, "blobs.forked")
    rows(ctx, "rows.child", child)
    reads(ctx, "read.child", child)
    _, exported = ctx.call("export", "GET", f"/api/experimental/session/{sid}/export", record=False)
    data = (exported or {}).get("data", exported)
    if isinstance(data, dict) and data.get("info"):
        fresh_ids(data)
        _, imported = ctx.call("import", "POST", "/api/experimental/session/import",
                               {**data, "location": {"directory": ctx.project}})
        rows(ctx, "rows.imported", "ses_000000000000importimportb")
        blobs(ctx, "blobs.imported")
        ctx.call("delete.imported", "DELETE", "/api/session/ses_000000000000importimportb")
    ctx.call("pending", "POST", f"/api/session/{sid}/prompt", {"text": "later", "files": [{"uri": PNG_URI}], "resume": False})
    _, inbox = ctx.call("inbox", "GET", f"/api/session/{sid}/inbox", record=False)
    pending = [i["id"] for i in (inbox or {}).get("data", [])]
    blobs(ctx, "blobs.pending")
    for i in pending:
        ctx.call("cancel", "DELETE", f"/api/session/{sid}/inbox/{i}")
    blobs(ctx, "blobs.cancelled")
    ctx.call("revert.stage", "POST", f"/api/session/{sid}/revert/stage", {"messageID": first})
    ctx.call("revert.commit", "POST", f"/api/session/{sid}/revert/commit", {})
    blobs(ctx, "blobs.reverted")
    rows(ctx, "rows.reverted", sid)
    ctx.call("delete.child", "DELETE", f"/api/session/{child}")
    blobs(ctx, "blobs.deleted")


@scenario("media")
def media_blob_fork_off_parent(ctx):
    """a fork copies media inline when the parent's location has media_blob off; an import into a location with it
    on stores the import's own inline media and refs only those blobs."""
    other = os.path.join(ctx.work, "other")
    os.makedirs(other, exist_ok=True)
    with open(os.path.join(other, "opencode.json"), "w") as f:
        json.dump(ON, f)
    setup(ctx, config={})
    sid = ctx.create_session()
    ctx.prompt(sid, "look", files=[{"uri": PNG_URI}])
    _, forked = ctx.call("fork", "POST", f"/api/session/{sid}/fork", {})
    child = (forked or {}).get("data", {}).get("id")
    rows(ctx, "rows.child", child)
    blobs(ctx, "blobs.forked")
    _, exported = ctx.call("export", "GET", f"/api/experimental/session/{sid}/export", record=False)
    data = (exported or {}).get("data", exported)
    if isinstance(data, dict) and data.get("info"):
        fresh_ids(data)
        ctx.call("import", "POST", "/api/experimental/session/import", {**data, "location": {"directory": other}})
        rows(ctx, "rows.imported", "ses_000000000000importimportb")
        blobs(ctx, "blobs.imported")

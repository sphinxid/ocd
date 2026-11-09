"""Opt-in artifacts (`experimental.artifacts`): the HTTP routes with the local provider (view redirect and headers,
files on disk, KV records), the `artifact_*` tools (direct and through Code Mode) and plugin providers
(`ctx.experimental.artifact.transform`, the `artifact-plugin` fixture)."""
import glob
import http.client
import json
import os
import sqlite3
import urllib.parse

from parity import harness
from parity.plugins import BUN, plugin_files
from parity.registry import scenario

ON = {"experimental": {"artifacts": True}}
VIEW_HEADERS = ("content-type", "access-control-allow-origin", "cache-control", "content-security-policy",
                "x-content-type-options", "location", "content-disposition", "etag")


def raw(ctx, label, method, path, headers=None):
    """A request that does not follow redirects; records status, the headers that matter and the body."""
    u = urllib.parse.urlparse(ctx.server_url)
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    h = {"authorization": ctx.server.auth, "x-opencode-directory": ctx.project}
    h.update(headers or {})
    conn.request(method, path, headers=h)
    resp = conn.getresponse()
    body = resp.read()
    conn.close()
    try:
        decoded = json.loads(body) if body else None
    except Exception:
        decoded = body.decode("utf-8", "replace")
    out = {"status": resp.status, "headers": {k: resp.getheader(k) for k in VIEW_HEADERS if resp.getheader(k) is not None},
           "body": decoded}
    ctx.rec(label, out)
    return out


def disk(ctx, label):
    """Files under `<data>/artifacts` (relative path → content), staging/backup directories included."""
    root = os.path.join(ctx.state, "data", "opencode", "artifacts")
    out = {}
    for base, dirs, files in os.walk(root):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(base, name)
            with open(path, "rb") as f:
                out[os.path.relpath(path, root)] = f.read().decode("utf-8", "replace")
    ctx.rec(label, out)


def database(ctx):
    """The server's database (upstream's detached build names it after its channel)."""
    for db in sorted(glob.glob(os.path.join(ctx.state, "data", "opencode", "opencode*.db"))):
        con = sqlite3.connect(db)
        try:
            if con.execute("SELECT 1 FROM sqlite_master WHERE name = 'kv'").fetchone():
                return db
        finally:
            con.close()
    return None


def kv(ctx, label):
    con = sqlite3.connect(database(ctx))
    try:
        rows = con.execute("SELECT key, value FROM kv WHERE key LIKE 'artifact:%' ORDER BY time_created, key").fetchall()
    finally:
        con.close()
    ctx.rec(label, [{"key": k, "value": json.loads(v)} for k, v in rows])


def created_id(data):
    return (data or {}).get("data", {}).get("id") if isinstance(data, dict) else None


@scenario("artifact", hold_for_title=0)
def artifact_http(ctx):
    """local provider over HTTP: providers, create (validation, entry rules), list, get, view (308, entry, files,
    headers, missing), update, delete; files on disk and KV records."""
    ctx.set_script([])
    ctx.make_project(config_extra=ON)
    ctx.start()
    sid = ctx.create_session(title="Artifacts")
    ctx.call("providers", "GET", "/api/experimental/artifacts/providers")
    ctx.call("list.empty", "GET", "/api/experimental/artifacts")
    site = {"index.html": "<h1>Hi</h1>\n", "assets/app.js": "console.log(1)\n", "./data.json": "{\"a\":1}",
            "img/dot.png": "\u0089PNG", "notes": "plain text"}
    for label, body in [
        ("create.blank.title", {"title": "  ", "files": site}),
        ("create.no.files", {"title": "t", "files": {}}),
        ("create.bad.path", {"title": "t", "files": {"a/../b": "x"}}),
        ("create.bad.path.empty", {"title": "t", "files": {"": "x"}}),
        ("create.no.entry", {"title": "t", "files": {"a.txt": "a", "b.txt": "b"}}),
        ("create.bad.entry", {"title": "t", "files": {"a.txt": "a"}, "entry": "./missing.txt"}),
        ("create.provider.missing", {"title": "t", "files": {"a.txt": "a"}, "provider": "nope"}),
        ("create.session.missing", {"title": "t", "files": {"a.txt": "a"}, "sessionID": "ses_000000000000aaaaaaaaaaaaaa"}),
    ]:
        ctx.call(label, "POST", "/api/experimental/artifacts", body)
    _, created = ctx.call("create.site", "POST", "/api/experimental/artifacts",
                          {"title": "  My site  ", "files": site, "sessionID": sid})
    art = created_id(created)
    _, single = ctx.call("create.single", "POST", "/api/experimental/artifacts",
                         {"title": "Readme", "files": {"/docs\\README.md": "# Hello\n"}, "provider": " local "})
    other = created_id(single)
    ctx.call("create.index.md", "POST", "/api/experimental/artifacts",
             {"title": "Md", "files": {"index.md": "# x", "z.txt": "z"}})
    disk(ctx, "disk.created")
    kv(ctx, "kv.created")
    ctx.call("list", "GET", "/api/experimental/artifacts")
    ctx.call("list.session", "GET", f"/api/experimental/artifacts?sessionID={sid}")
    ctx.call("list.provider", "GET", "/api/experimental/artifacts?provider=nope")
    ctx.call("list.session.missing", "GET", "/api/experimental/artifacts?sessionID=ses_000000000000aaaaaaaaaaaaaa")
    ctx.call("get", "GET", f"/api/experimental/artifacts/{art}")
    ctx.call("get.missing", "GET", "/api/experimental/artifacts/art_000000000000aaaaaaaaaaaaaa")
    base = f"/api/experimental/artifacts/{art}/view"
    raw(ctx, "view.bare", "GET", base)
    raw(ctx, "view.bare.query", "GET", base + "?x=1&y=two")
    raw(ctx, "view.slash", "GET", base + "/")
    raw(ctx, "view.js", "GET", base + "/assets/app.js")
    raw(ctx, "view.json", "GET", base + "/data.json")
    raw(ctx, "view.png", "GET", base + "/img/dot.png")
    raw(ctx, "view.noext", "GET", base + "/notes")
    raw(ctx, "view.encoded", "GET", base + "/assets%2Fapp.js")
    raw(ctx, "view.missing", "GET", base + "/nope.html")
    raw(ctx, "view.dir", "GET", base + "/assets")
    raw(ctx, "view.dotdot", "GET", base + "/a/%2E%2E/index.html")
    raw(ctx, "view.unknown", "GET", "/api/experimental/artifacts/art_000000000000aaaaaaaaaaaaaa/view")
    raw(ctx, "view.unknown.slash", "GET", "/api/experimental/artifacts/art_000000000000aaaaaaaaaaaaaa/view/")
    raw(ctx, "view.other", "GET", f"/api/experimental/artifacts/{other}/view/")
    raw(ctx, "view.no.header", "GET", base + "/", headers={"x-opencode-directory": ""})
    for label, body in [
        ("update.bad.entry", {"files": {"a.txt": "a", "b.txt": "b"}}),
        ("update.blank.title", {"title": " ", "files": {"index.html": "x"}}),
        ("update.session.missing", {"files": {"index.html": "x"}, "sessionID": "ses_000000000000aaaaaaaaaaaaaa"}),
        ("update.keep.entry", {"files": {"index.html": "<h1>v2</h1>", "extra.css": "p{}"}}),
        ("update.title.entry", {"title": " Renamed ", "files": {"main.html": "<p>m</p>", "index.html": "i"},
                                "entry": "main.html"}),
    ]:
        ctx.call(label, "PATCH", f"/api/experimental/artifacts/{art}", body)
    ctx.call("update.missing", "PATCH", "/api/experimental/artifacts/art_000000000000aaaaaaaaaaaaaa",
             {"files": {"a": "b"}})
    raw(ctx, "view.after.update", "GET", base + "/")
    raw(ctx, "view.old.file", "GET", base + "/assets/app.js")
    ctx.call("get.after.update", "GET", f"/api/experimental/artifacts/{art}")
    disk(ctx, "disk.updated")
    ctx.call("delete", "DELETE", f"/api/experimental/artifacts/{art}")
    ctx.call("delete.again", "DELETE", f"/api/experimental/artifacts/{art}")
    ctx.call("get.deleted", "GET", f"/api/experimental/artifacts/{art}")
    raw(ctx, "view.deleted", "GET", base + "/")
    ctx.call("list.after.delete", "GET", "/api/experimental/artifacts")
    disk(ctx, "disk.deleted")
    kv(ctx, "kv.deleted")
    ctx.record_events(filt=lambda e: e.get("type", "").startswith("artifact"))


@scenario("artifact", hold_for_title=0)
def artifact_locations(ctx):
    """route locations: a stored artifact answers from its own directory without one in the request, a session's
    location wins over the request's, another project neither lists nor reads it, a project with artifacts off
    answers 403."""
    ctx.set_script([])
    other = os.path.join(ctx.work, "other")
    off = os.path.join(ctx.work, "off")
    for d, cfg in ((other, ON), (off, {})):
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "opencode.json"), "w") as f:
            json.dump(cfg, f)
    harness.git(other, "init", "-q")
    harness.git(other, "commit", "-q", "--allow-empty", "-m", "root")
    ctx.make_project(config_extra=ON, git_repo=True)
    ctx.start()
    sid = ctx.create_session(title="Here")
    _, created = ctx.call("create", "POST", "/api/experimental/artifacts", {"title": "t", "files": {"a.txt": "a"}})
    art = created_id(created)
    nowhere = {}
    ctx.call("get.no.location", "GET", f"/api/experimental/artifacts/{art}", headers=nowhere)
    raw(ctx, "view.no.location", "GET", f"/api/experimental/artifacts/{art}/view/", headers={"x-opencode-directory": ""})
    ctx.call("get.other.project", "GET", f"/api/experimental/artifacts/{art}", headers={"x-opencode-directory": other})
    ctx.call("list.other.project", "GET", "/api/experimental/artifacts", headers={"x-opencode-directory": other})
    ctx.call("providers.off", "GET", "/api/experimental/artifacts/providers", headers={"x-opencode-directory": off})
    ctx.call("create.session.location", "POST", "/api/experimental/artifacts",
             {"title": "s", "files": {"b.txt": "b"}, "sessionID": sid}, headers={})
    ctx.call("list.session.off.header", "GET", f"/api/experimental/artifacts?sessionID={sid}",
             headers={"x-opencode-directory": off})
    kv(ctx, "kv")


def _tools_project(ctx, script, config_extra=None, plugins=None):
    cfg = dict(ON)
    cfg.update(config_extra or {})
    files = None
    if plugins:
        ctx.env_extra["OCD_BUN_PATH"] = BUN
        cfg["plugins"] = plugins
        files = plugin_files("artifact-plugin")
    ctx.set_script(script)
    ctx.make_project(config_extra=cfg, files=files)
    ctx.start()


@scenario("artifact")
def artifact_tools_direct(ctx):
    """`execute` denied, so the artifact tools are direct: create (and a failing one), read, update, delete, a
    missing ID; tool definitions sent to the model, stored parts and the files the tools wrote."""
    create = {"title": "Page", "files": {"index.html": "<p>tool</p>", "style.css": "p{}"}}
    _tools_project(ctx, [
        {"tools": [{"name": "opencode_artifact_create", "id": "call_create", "args": create}]},
        {"tools": [{"name": "opencode_artifact_create", "id": "call_bad", "args": {"title": "x", "files": {"a": "1", "b": "2"}}}]},
        {"tools": [{"name": "opencode_artifact_create", "id": "call_invalid", "args": {"title": "x"}}]},
        {"text": "Created."},
    ], config_extra={"permission": {"execute": "deny"}})
    sid = ctx.create_session()
    ctx.prompt(sid, "publish")
    _, data = ctx.call("list", "GET", "/api/experimental/artifacts")
    art = ((data or {}).get("data") or [{}])[0].get("id", "art_missing")
    ctx.set_script([
        {"tools": [{"name": "opencode_artifact_read", "id": "call_read", "args": {"id": art}}]},
        {"tools": [{"name": "opencode_artifact_update", "id": "call_update",
                    "args": {"id": art, "files": {"index.html": "<p>v2</p>"}, "title": "Page 2"}}]},
        {"tools": [{"name": "opencode_artifact_read", "id": "call_read_bad", "args": {"id": "nope"}}]},
        {"tools": [{"name": "opencode_artifact_delete", "id": "call_delete", "args": {"id": art}}]},
        {"tools": [{"name": "opencode_artifact_delete", "id": "call_delete_again", "args": {"id": art}}]},
        {"text": "Done."},
    ])
    ctx.prompt(sid, "edit")
    ctx.call("messages", "GET", f"/api/session/{sid}/message?order=asc")
    disk(ctx, "disk")
    ctx.record_llm(full=True)


@scenario("artifact")
def artifact_tools_codemode(ctx):
    """the artifact tools through Code Mode (`tools.opencode.artifact_*`), and off: not offered without
    `experimental.artifacts`."""
    code = ("const a = await tools.opencode.artifact_create({ title: 'Code', files: { 'index.html': '<b>c</b>' } });\n"
            "const r = await tools.opencode.artifact_read({ id: a.id });\n"
            "const u = await tools.opencode.artifact_update({ id: a.id, files: { 'x.md': '# x' } });\n"
            "const d = await tools.opencode.artifact_delete({ id: a.id });\n"
            "const t = (x) => ({ ...x, time: typeof x.time.created });\n"
            "return { a: t(a), r: { info: t(r.info), files: r.files }, u: t(u), d };")
    _tools_project(ctx, [
        {"tools": [{"name": "execute", "id": "call_code", "args": {"code": code}}]},
        {"text": "Done."},
    ])
    sid = ctx.create_session()
    ctx.prompt(sid, "code")
    ctx.call("messages", "GET", f"/api/session/{sid}/message?order=asc")
    ctx.record_llm(full=True)


@scenario("artifact")
def artifact_plugin_provider(ctx):
    """plugin providers: listed and named in artifact_create's description; create/read/update/delete through
    `pages` (its URL, a kept URL), failures of `broken` (Error, non-Error, empty message), the default moved to
    `pages`, `artifact.updated`."""
    _tools_project(ctx, [
        {"tools": [{"name": "opencode_artifact_create", "id": "call_pages",
                    "args": {"title": "Plug", "files": {"index.html": "<i>p</i>"}}}]},
        {"tools": [{"name": "opencode_artifact_create", "id": "call_broken",
                    "args": {"title": "Broken", "files": {"a.txt": "a"}, "provider": "broken"}}]},
        {"text": "Done."},
    ], config_extra={"permission": {"execute": "deny"}},
        plugins=[{"package": "./plugins/artifact-plugin", "options": {"default": "pages"}}])
    sid = ctx.create_session()
    ctx.settle(f"/api/experimental/artifacts/providers?directory={ctx.project}", timeout=10)
    ctx.call("providers", "GET", "/api/experimental/artifacts/providers")
    ctx.prompt(sid, "publish")
    _, data = ctx.call("list", "GET", "/api/experimental/artifacts")
    items = (data or {}).get("data") or []
    art = items[0]["id"] if items else "art_missing"
    ctx.call("get", "GET", f"/api/experimental/artifacts/{art}")
    ctx.call("update", "PATCH", f"/api/experimental/artifacts/{art}", {"files": {"b.html": "b", "index.html": "i2"}})
    ctx.call("update.keep.url", "PATCH", f"/api/experimental/artifacts/{art}",
             {"title": "keep url", "files": {"index.html": "i3"}})
    raw(ctx, "view.not.local", "GET", f"/api/experimental/artifacts/{art}/view/")
    _, local = ctx.call("create.local", "POST", "/api/experimental/artifacts",
                        {"title": "L", "files": {"index.html": "l"}, "provider": "local"})
    ctx.call("create.broken", "POST", "/api/experimental/artifacts",
             {"title": "B", "files": {"index.html": "b"}, "provider": "broken"})
    _, b = ctx.call("create.pages.for.broken", "POST", "/api/experimental/artifacts",
                    {"title": "B2", "files": {"index.html": "b"}})
    ctx.call("delete", "DELETE", f"/api/experimental/artifacts/{art}")
    ctx.call("get.deleted", "GET", f"/api/experimental/artifacts/{art}")
    ctx.call("list.after", "GET", "/api/experimental/artifacts")
    kv(ctx, "kv")
    ctx.call("messages", "GET", f"/api/session/{sid}/message?order=asc")
    ctx.record_llm(full=True)
    ctx.record_events(filt=lambda e: e.get("type", "").endswith(".updated") and not e.get("type", "").startswith("session."))


@scenario("artifact", hold_for_title=0)
def artifact_plugin_failures(ctx):
    """a plugin provider's read/update/delete failures map to 503 with the cause's message (or none), and a
    default that names no provider falls back to `local`."""
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script([])
    cfg = dict(ON)
    cfg["plugins"] = [{"package": "./plugins/artifact-plugin", "options": {"default": "missing"}}]
    ctx.make_project(config_extra=cfg, files=plugin_files("artifact-plugin"))
    ctx.start()
    ctx.create_session()
    ctx.settle(f"/api/experimental/artifacts/providers?directory={ctx.project}", timeout=10)
    ctx.call("providers", "GET", "/api/experimental/artifacts/providers")
    _, created = ctx.call("create.default", "POST", "/api/experimental/artifacts", {"title": "D", "files": {"x.txt": "x"}})
    # Records made by "broken" cannot be created through it, so write one the way the service stores it.
    db = database(ctx)
    info = dict((created or {}).get("data") or {})
    info["id"] = "art_000000000000bbbbbbbbbbbbbb"
    info["provider"] = "broken"
    con = sqlite3.connect(db)
    try:
        project = con.execute("SELECT json_extract(value, '$.projectID') FROM kv WHERE key LIKE 'artifact:item:%'").fetchone()[0]
        con.execute("INSERT INTO kv (key, value, time_created, time_updated) VALUES (?, ?, 1, 1)",
                    (f"artifact:item:{info['id']}", json.dumps({"info": info, "directory": ctx.project, "projectID": project})))
        con.commit()
    finally:
        con.close()
    ctx.call("get.broken", "GET", f"/api/experimental/artifacts/{info['id']}")
    ctx.call("update.broken", "PATCH", f"/api/experimental/artifacts/{info['id']}", {"files": {"x.txt": "y"}})
    ctx.call("delete.broken", "DELETE", f"/api/experimental/artifacts/{info['id']}")
    ctx.call("list", "GET", "/api/experimental/artifacts")

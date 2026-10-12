"""Session lifecycle, permissions and forms."""
import time

from parity.registry import scenario

TEXT = lambda t="Done.": {"text": t}  # noqa: E731


def boot(ctx, script=(), **project):
    ctx.set_script(list(script))
    ctx.make_project(**project)
    ctx.start()


@scenario("session", hold_for_title=0)
def session_crud(ctx):
    """create (explicit title), get, update, list with paging/order/search/parentID, remove, 404 after remove."""
    boot(ctx)
    ids = [ctx.create_session(f"create{i}", title=f"Session {i}") for i in range(3)]
    child = ctx.create_session("create.child", parentID=ids[0])
    ctx.call("get", "GET", f"/api/session/{ids[0]}")
    ctx.call("update.title", "PATCH", f"/api/session/{ids[1]}", {"title": "Renamed"})
    ctx.call("update.metadata", "PATCH", f"/api/session/{ids[1]}", {"metadata": {"k": "v"}})
    ctx.call("update.permissions", "PATCH", f"/api/session/{ids[1]}",
             {"permissions": [{"action": "shell", "resource": "*", "effect": "deny"}]})
    ctx.call("get.updated", "GET", f"/api/session/{ids[1]}")
    ctx.call("list", "GET", "/api/session")
    ctx.call("list.directory", "GET", f"/api/session?directory={ctx.project}")
    ctx.call("list.roots", "GET", "/api/session?parentID=null")
    ctx.call("list.children", "GET", f"/api/session?parentID={ids[0]}")
    ctx.call("list.search", "GET", "/api/session?search=Renamed")
    ctx.call("list.asc", "GET", "/api/session?order=asc")
    _, page = ctx.call("list.page1", "GET", "/api/session?limit=2")
    nxt = (page or {}).get("cursor", {}).get("next")
    if nxt:
        _, page2 = ctx.call("list.page2", "GET", f"/api/session?cursor={nxt}")
        prev = (page2 or {}).get("cursor", {}).get("previous")
        if prev:
            ctx.call("list.page2.prev", "GET", f"/api/session?cursor={prev}")
    ctx.call("list.badcursor", "GET", "/api/session?cursor=notacursor")
    ctx.call("list.badlimit", "GET", "/api/session?limit=0")
    ctx.call("active", "GET", "/api/session/active")
    ctx.call("remove", "DELETE", f"/api/session/{ids[2]}")
    ctx.call("get.removed", "GET", f"/api/session/{ids[2]}")
    ctx.call("remove.again", "DELETE", f"/api/session/{ids[2]}")
    ctx.call("remove.parent", "DELETE", f"/api/session/{ids[0]}")
    ctx.call("get.child.after.parent.removed", "GET", f"/api/session/{child}")
    ctx.call("list.final", "GET", "/api/session")
    time.sleep(0.5)
    ctx.record_events()


@scenario("session")
def session_messages(ctx):
    """Two prompts; message list (paging), message get, context, inbox, log, export."""
    boot(ctx, [TEXT("First answer."), TEXT("Second answer.")])
    sid = ctx.create_session()
    ctx.prompt(sid, "one")
    ctx.prompt(sid, "two")
    _, msgs = ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("messages.limit", "GET", f"/api/session/{sid}/message?limit=2")
    _, page = ctx.call("messages.limit.desc", "GET", f"/api/session/{sid}/message?limit=2&order=desc")
    cur = (page or {}).get("cursor", {}).get("next")
    if cur:
        ctx.call("messages.page2", "GET", f"/api/session/{sid}/message?cursor={cur}")
    items = (msgs or {}).get("data", [])
    if items:
        ctx.call("message.get", "GET", f"/api/session/{sid}/message/{items[0]['id']}")
    ctx.call("message.get.missing", "GET", f"/api/session/{sid}/message/msg_000000000000aaaaaaaaaaaaaa")
    ctx.call("context", "GET", f"/api/session/{sid}/context")
    ctx.call("inbox", "GET", f"/api/session/{sid}/inbox")
    ctx.call("diff", "GET", f"/api/session/{sid}/diff")
    ctx.call("log", "GET", f"/api/experimental/session/{sid}/log")
    ctx.call("export", "GET", f"/api/experimental/session/{sid}/export")
    ctx.call("export.sanitized", "GET", f"/api/experimental/session/{sid}/export?sanitize=true")
    ctx.call("instructions.entries", "GET", f"/api/experimental/session/{sid}/instructions/entries")
    ctx.call("stats", "GET", "/api/experimental/session/stats?tools=detail&timezone=UTC")
    ctx.call("stats.summary", "GET", "/api/experimental/session/stats?tools=summary&timezone=UTC")
    ctx.call("stats.none", "GET", "/api/experimental/session/stats?tools=none&timezone=UTC")
    ctx.record_events()


@scenario("session")
def session_import(ctx):
    """export a session, import it back (new id and same id conflict)."""
    boot(ctx, [TEXT("Exported.")])
    sid = ctx.create_session()
    ctx.prompt(sid, "export me")
    _, exp = ctx.call("export", "GET", f"/api/experimental/session/{sid}/export", record=False)
    data = (exp or {}).get("data", exp)
    ctx.call("import.conflict", "POST", "/api/experimental/session/import", {"data": data})
    ctx.call("remove", "DELETE", f"/api/session/{sid}")
    ctx.call("import", "POST", "/api/experimental/session/import", {"data": data})
    ctx.call("get", "GET", f"/api/session/{sid}")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")


@scenario("session")
def session_fork(ctx):
    """fork a whole session and fork before the second user message."""
    boot(ctx, [TEXT("A1"), TEXT("A2")])
    sid = ctx.create_session()
    ctx.prompt(sid, "one")
    ctx.prompt(sid, "two")
    _, msgs = ctx.call("messages", "GET", f"/api/session/{sid}/message", record=False)
    users = [m for m in (msgs or {}).get("data", []) if m.get("type") == "user"]
    f1 = ctx.data("fork.all", "POST", f"/api/session/{sid}/fork", {})
    ctx.call("fork.all.messages", "GET", f"/api/session/{f1['id']}/message")
    if len(users) > 1:
        f2 = ctx.data("fork.before", "POST", f"/api/session/{sid}/fork", {"before": users[1]["id"]})
        ctx.call("fork.before.messages", "GET", f"/api/session/{f2['id']}/message")
    ctx.call("fork.badmessage", "POST", f"/api/session/{sid}/fork", {"before": "msg_000000000000aaaaaaaaaaaaaa"})
    ctx.call("list", "GET", "/api/session")
    ctx.record_events()


@scenario("session")
def session_revert(ctx):
    """write tool changes a file in a git repo; revert stage (file restored), clear, stage again, commit."""
    boot(ctx, [
        {"tools": [{"name": "write", "args": {"path": "$PROJECT/a.txt", "content": "changed\n"}}]},
        TEXT("Wrote it."),
        TEXT("Second."),
    ], files={"a.txt": "original\n"}, git_repo=True)
    sid = ctx.create_session()
    ctx.prompt(sid, "change a.txt")
    ctx.prompt(sid, "second")
    _, msgs = ctx.call("messages", "GET", f"/api/session/{sid}/message", record=False)
    users = {m["text"]: m for m in (msgs or {}).get("data", []) if m.get("type") == "user"}
    users = [users["change a.txt"], users["second"]]
    ctx.call("diff", "GET", f"/api/session/{sid}/diff")
    read = lambda: open(ctx.project + "/a.txt").read()  # noqa: E731
    ctx.rec("file.after.write", read())
    ctx.call("revert.stage", "POST", f"/api/session/{sid}/revert/stage", {"messageID": users[0]["id"], "files": True})
    ctx.rec("file.after.stage", read())
    ctx.call("session.staged", "GET", f"/api/session/{sid}")
    mark = len(ctx.server.events)
    ctx.call("revert.clear", "DELETE", f"/api/session/{sid}/revert")
    ctx.wait_idle(sid, start=mark, timeout=10)  # clearing resumes the loop (no-op run)
    ctx.rec("file.after.clear", read())
    ctx.call("revert.stage2", "POST", f"/api/session/{sid}/revert/stage", {"messageID": users[1]["id"]})
    ctx.rec("file.after.stage2", read())
    ctx.call("revert.commit", "POST", f"/api/session/{sid}/revert/commit")
    ctx.call("messages.after.commit", "GET", f"/api/session/{sid}/message")
    ctx.call("revert.stage.missing", "POST", f"/api/session/{sid}/revert/stage",
             {"messageID": "msg_000000000000aaaaaaaaaaaaaa"})
    ctx.record_events()


@scenario("session")
def session_interrupt(ctx):
    """a hanging model stream is interrupted; then the session answers a new prompt."""
    boot(ctx, [{"text": "partial", "hang": True}, TEXT("After interrupt.")])
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "hang please", wait=False)
    ctx.wait_for("session.step.started", start=mark, timeout=20)
    time.sleep(0.5)
    ctx.call("active.busy", "GET", "/api/session/active")
    ctx.call("interrupt", "POST", f"/api/session/{sid}/interrupt")
    ctx.wait_idle(sid, start=mark)
    ctx.call("interrupt.idle", "POST", f"/api/session/{sid}/interrupt")
    ctx.prompt(sid, "again")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()


@scenario("session")
def session_wait(ctx):
    """experimental wait blocks until the loop is idle; wait on an idle session returns at once."""
    boot(ctx, [{"text": "slow", "delay": 1.5}])
    sid = ctx.create_session()
    ctx.call("wait.idle.before", "POST", f"/api/experimental/session/{sid}/wait")
    ctx.prompt(sid, "go", wait=False)
    ctx.call("wait", "POST", f"/api/experimental/session/{sid}/wait")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("wait.missing", "POST", "/api/experimental/session/ses_000000000000aaaaaaaaaaaaaa/wait")


@scenario("session", hold_for_title=0)
def session_switch(ctx):
    """switch agent and model, then prompt; invalid agent/model."""
    boot(ctx, [TEXT("Planned.")])
    sid = ctx.create_session(title="Fixed title")
    ctx.call("agent.plan", "POST", f"/api/session/{sid}/agent", {"agent": "plan"})
    ctx.call("agent.missing", "POST", f"/api/session/{sid}/agent", {"agent": "nope"})
    ctx.call("model", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "mock", "id": "m1"}})
    ctx.call("model.missing", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "mock", "id": "zzz"}})
    ctx.prompt(sid, "plan something")
    ctx.call("get", "GET", f"/api/session/{sid}")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


def _env_read_script():
    return [{"tools": [{"name": "read", "args": {"path": "$PROJECT/.env"}}]}, TEXT("Read the env.")]


def _permission_flow(ctx, decision, message=None, second=False):
    boot(ctx, _env_read_script() + (_env_read_script() if second else []), files={".env": "SECRET=1\n"})
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "read .env", wait=False)
    ev = ctx.wait_for("permission.asked", start=mark, timeout=30)
    ctx.call("permission.list", "GET", f"/api/session/{sid}/permission")
    ctx.call("permission.request.list", "GET", f"/api/permission/request?directory={ctx.project}")
    if ev:
        pid = ev["data"]["id"]
        ctx.call("permission.get", "GET", f"/api/session/{sid}/permission/{pid}")
        body = {"decision": decision}
        if message:
            body["message"] = message
        ctx.call("permission.reply", "POST", f"/api/session/{sid}/permission/{pid}/reply", body)
        ctx.call("permission.reply.again", "POST", f"/api/session/{sid}/permission/{pid}/reply", body)
    ctx.wait_idle(sid, start=mark)
    if second:
        mark = len(ctx.server.events)
        ctx.prompt(sid, "read .env again", wait=False)
        ctx.wait_idle(sid, start=mark)
    ctx.call("permission.saved", "GET", "/api/permission/saved")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("permission")
def permission_once(ctx):
    """read .env asks; reply once."""
    _permission_flow(ctx, "once")


@scenario("permission")
def permission_always(ctx):
    """read .env asks; reply always (saved), second read is auto-approved."""
    _permission_flow(ctx, "always", second=True)


@scenario("permission")
def permission_reject(ctx):
    """read .env asks; reject without message."""
    _permission_flow(ctx, "reject")


@scenario("permission")
def permission_reject_message(ctx):
    """read .env asks; reject with corrective feedback."""
    _permission_flow(ctx, "reject", message="Do not read secrets.")


@scenario("permission", hold_for_title=0)
def permission_api(ctx):
    """client-created permission requests: create/list/get/reply, saved list/remove, errors."""
    boot(ctx)
    sid = ctx.create_session(title="Perms")
    p = ctx.data("create", "POST", f"/api/session/{sid}/permission",
                 {"action": "custom", "resources": ["thing"], "save": ["thing"]})
    ctx.call("list", "GET", f"/api/session/{sid}/permission")
    if isinstance(p, dict) and p.get("id"):
        ctx.call("get", "GET", f"/api/session/{sid}/permission/{p['id']}")
        ctx.call("reply.bad", "POST", f"/api/session/{sid}/permission/{p['id']}/reply", {"decision": "maybe"})
        ctx.call("reply", "POST", f"/api/session/{sid}/permission/{p['id']}/reply", {"decision": "always"})
    _, saved = ctx.call("saved", "GET", "/api/permission/saved")
    for s in (saved or {}).get("data", []):
        ctx.call("saved.remove", "DELETE", f"/api/permission/saved/{s['id']}")
    ctx.call("saved.after", "GET", "/api/permission/saved")
    ctx.call("get.missing", "GET", f"/api/session/{sid}/permission/per_000000000000aaaaaaaaaaaaaa")
    ctx.call("reply.missing", "POST", f"/api/session/{sid}/permission/per_000000000000aaaaaaaaaaaaaa/reply",
             {"decision": "once"})
    time.sleep(0.3)
    ctx.record_events()


FORM = {"title": "Pick", "fields": [
    {"key": "name", "type": "string", "title": "Name", "required": True, "minLength": 2},
    {"key": "count", "type": "integer", "minimum": 1, "maximum": 5},
    {"key": "ok", "type": "boolean", "default": True},
    {"key": "tags", "type": "multiselect", "options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}]},
]}


@scenario("form", hold_for_title=0)
def form_api(ctx):
    """forms: create, list, get, invalid answer, reply, reply again, cancel, global list."""
    boot(ctx)
    sid = ctx.create_session(title="Forms")
    f = ctx.data("create", "POST", f"/api/session/{sid}/form", FORM)
    ctx.call("list", "GET", f"/api/session/{sid}/form")
    ctx.call("list.global", "GET", f"/api/form?directory={ctx.project}")
    fid = (f or {}).get("id") if isinstance(f, dict) else None
    if fid:
        ctx.call("get", "GET", f"/api/session/{sid}/form/{fid}")
        ctx.call("reply.invalid", "POST", f"/api/session/{sid}/form/{fid}/reply", {"answer": {"name": "x", "count": 9}})
        ctx.call("reply.unknownkey", "POST", f"/api/session/{sid}/form/{fid}/reply", {"answer": {"name": "xy", "zzz": 1}})
        ctx.call("reply", "POST", f"/api/session/{sid}/form/{fid}/reply",
                 {"answer": {"name": "Bob", "count": 2, "ok": False, "tags": ["a"]}})
        ctx.call("reply.again", "POST", f"/api/session/{sid}/form/{fid}/reply", {"answer": {"name": "Bob"}})
        ctx.call("get.answered", "GET", f"/api/session/{sid}/form/{fid}")
    f2 = ctx.data("create2", "POST", f"/api/session/{sid}/form", {"title": "Other", "fields": [{"key": "x", "type": "string"}]})
    if isinstance(f2, dict) and f2.get("id"):
        ctx.call("cancel", "DELETE", f"/api/session/{sid}/form/{f2['id']}")
        ctx.call("cancel.again", "DELETE", f"/api/session/{sid}/form/{f2['id']}")
    ctx.call("create.invalid", "POST", f"/api/session/{sid}/form", {"title": "Bad", "fields": [{"key": "x", "type": "nope"}]})
    ctx.call("get.missing", "GET", f"/api/session/{sid}/form/frm_000000000000aaaaaaaaaaaaaa")
    time.sleep(0.3)
    ctx.record_events()


@scenario("form")
def form_question_tool(ctx):
    """the question tool opens a form; the client answers it; the model receives the answer."""
    boot(ctx, [
        {"tools": [{"name": "question", "args": {"questions": [{"question": "Which color?", "header": "Color",
                                                                 "options": [{"label": "Red", "description": "warm"},
                                                                             {"label": "Blue", "description": "cool"}]}]}}]},
        TEXT("You chose."),
    ])
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "ask me", wait=False)
    ev = ctx.wait_for("form.created", start=mark, timeout=30)
    if ev:
        form = ev["data"]["form"]
        ctx.call("form.get", "GET", f"/api/session/{sid}/form/{form['id']}")
        key = form["fields"][0]["key"]
        ctx.call("form.reply", "POST", f"/api/session/{sid}/form/{form['id']}/reply", {"answer": {key: "Blue"}})
    ctx.wait_idle(sid, start=mark)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()

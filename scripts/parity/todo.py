"""The experimental todo list (`experimental.todo`, the `todowrite` tool of the `opencode.tool.todo` plugin)."""
import json
import time

from parity.registry import scenario

TODO_ON = {"experimental": {"todo": True}}

FIRST = [
    {"content": "Read the code", "status": "completed"},
    {"content": "Write \"tests\"", "status": "in_progress"},
    {"content": "Ship it", "status": "pending"},
]
SECOND = [
    {"content": "Read the code", "status": "completed"},
    {"content": "Write \"tests\"", "status": "completed"},
    {"content": "Ship it", "status": "in_progress"},
    {"content": "Rewrite everything", "status": "cancelled"},
]


def _todo(todos, id=None):
    call = {"name": "todowrite", "args": {"todos": todos}}
    if id:
        call["id"] = id
    return call


def _record(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.call("plugin.list", "GET", f"/api/plugin?directory={ctx.project}")


@scenario("todo")
def todo_loop(ctx):
    """`experimental.todo` on: the model writes a list, sends an invalid status, updates the list (cancelled item,
    quotes), clears it and answers. Full model requests (tool definitions), tool results, stored parts, events."""
    ctx.set_script([
        {"tools": [_todo(FIRST)]},
        {"tools": [_todo([{"content": "Bad", "status": "done"}])]},
        {"tools": [_todo(SECOND)]},
        {"tools": [_todo([])]},
        {"text": "All done."},
    ])
    ctx.make_project(config_extra=TODO_ON)
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "plan the work")
    _record(ctx, sid)
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("todo")
def todo_off(ctx):
    """`experimental.todo` off (unset, then `false`): `todowrite` is not offered and a call to it is an unknown
    tool; `opencode.tool.todo` is still listed. Then the config file turns it on (file watching on, so the plugin's
    `config.updated` observer reloads the tools) and the next request offers it."""
    ctx.env_extra["OPENCODE_DISABLE_FILEWATCHER"] = "0"
    ctx.set_script([
        {"tools": [_todo(FIRST)]},
        {"text": "No todo tool."},
        {"tools": [_todo(FIRST)]},
        {"text": "Now with todos."},
    ])
    ctx.make_project(config_extra={"experimental": {"todo": False}})
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "plan without todos")
    _record(ctx, sid)
    path = f"{ctx.project}/opencode.json"
    cfg = json.load(open(path))
    cfg["experimental"] = {"todo": True}
    mark = len(ctx.server.events)
    with open(path, "w") as f:
        f.write(json.dumps(cfg, indent=2))
    ctx.wait_for("config.updated", start=mark, timeout=15)
    time.sleep(1.0)
    ctx.prompt(sid, "plan with todos")
    ctx.call("messages.after", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: e.get("type", "").startswith("session."))
    ctx.record_llm()


@scenario("todo")
def todo_permission(ctx):
    """`todowrite` asks (`permission: {todowrite: "ask"}`): the ask is rejected with feedback ("Unable to update
    todos"), then the next call is answered "always" (saved as `todowrite *`)."""
    ctx.set_script([
        {"tools": [_todo(FIRST)]},
        {"tools": [_todo(SECOND)]},
        {"text": "Done."},
    ])
    ctx.make_project(config_extra={**TODO_ON, "permission": {"todowrite": "ask"}})
    ctx.start()
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "plan the work", wait=False)
    for decision, message in (("reject", "Not now."), ("always", None)):
        ev = ctx.wait_for("permission.asked", start=mark, timeout=30)
        if not ev:
            break
        mark = len(ctx.server.events)
        pid = ev["data"]["id"]
        ctx.call(f"permission.get.{decision}", "GET", f"/api/session/{sid}/permission/{pid}")
        body = {"decision": decision}
        if message:
            body["message"] = message
        ctx.call(f"permission.reply.{decision}", "POST", f"/api/session/{sid}/permission/{pid}/reply", body)
    ctx.wait_idle(sid, start=0)
    ctx.call("permission.saved", "GET", "/api/permission/saved")
    _record(ctx, sid)
    ctx.record_events()
    ctx.record_llm()

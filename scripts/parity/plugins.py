"""Server plugins: the oc-plugin fixture plugins (crates/oc-plugin/tests/fixtures) configured as local plugins of
the project. Upstream runs them in-process (Bun); the port runs them in its Bun sidecar (`OCD_BUN_PATH`)."""
import glob
import os
import sqlite3
import time

from parity.registry import scenario

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FIXTURES = os.path.join(ROOT, "crates", "oc-plugin", "tests", "fixtures")
BUN = os.environ.get("OCD_BUN_PATH") or os.environ.get("OCD_TEST_BUN") or \
    os.path.join(os.environ.get("OCD_SCRATCH", "/tmp/ocd-scratch"), "bun142/bun-linux-x64/bun")


def plugin_files(*names):
    out = {}
    for name in names:
        base = os.path.join(FIXTURES, name)
        for path in sorted(glob.glob(os.path.join(base, "**", "*"), recursive=True)):
            if os.path.isfile(path):
                with open(path) as f:
                    out[os.path.join("plugins", name, os.path.relpath(path, base))] = f.read()
    return out


def boot(ctx, plugins, names, script=None, files=None):
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script(script or [])
    ctx.make_project(files=dict(plugin_files(*names), **(files or {})), config_extra={"plugins": plugins})
    ctx.start()


def settle_lists(ctx):
    d = f"directory={ctx.project}"
    for path in ("/api/agent", "/api/command", "/api/skill", "/api/plugin"):
        ctx.settle(f"{path}?{d}", timeout=10)


def record_lists(ctx):
    d = f"directory={ctx.project}"
    for label, path in (("plugin.list", "/api/plugin"), ("command.list", "/api/command"), ("agent.list", "/api/agent"),
                        ("skill.list", "/api/skill")):
        ctx.call(label, "GET", f"{path}?{d}")


def record_tool_definitions(ctx, label="llm.tools"):
    """The tool definitions offered in the first model request (name, description, schema)."""
    reqs = ctx.mock.main_requests()
    body = reqs[0]["body"] if reqs else {}
    tools = sorted(((t.get("function") or {}) for t in (body.get("tools") or [])), key=lambda f: f.get("name", ""))
    ctx.rec(label, tools)


def record_system(ctx, label="llm.system"):
    """System parts of every model request that a plugin added (marker text only)."""
    out = []
    for r in ctx.mock.main_requests():
        msgs = r["body"].get("messages", []) if isinstance(r["body"], dict) else []
        out.append(["context hook was here" in str(m.get("content")) for m in msgs if m.get("role") == "system"])
    ctx.rec(label, out)


def record_storage(ctx, label="storage"):
    """Plugin storage rows (`plugin:<hex id>:` keys of the kv table)."""
    rows = []
    for db in sorted(glob.glob(os.path.join(ctx.state, "data", "opencode", "opencode*.db"))):
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            rows = con.execute("SELECT key, value FROM kv WHERE key LIKE 'plugin:%' ORDER BY key").fetchall()
            con.close()
        except sqlite3.Error as e:
            rows = [("error", str(e))]
    ctx.rec(label, [[k, v] for k, v in rows if "event:" not in k or "rpc." in k or "session." in k])


@scenario("plugin", hold_for_title=0)
def plugin_inventory(ctx):
    """local plugins (Promise, Effect, invalid, failing transform): plugin.list, commands, agents, skills, events."""
    boot(ctx, [{"package": "./plugins/promise-plugin", "options": {"suffix": "!"}}, "./plugins/effect-plugin",
               "./plugins/invalid", "./plugins/bad-transform"],
         ["promise-plugin", "effect-plugin", "invalid", "bad-transform"])
    ctx.create_session(title="Boot")
    settle_lists(ctx)
    record_lists(ctx)
    ctx.record_events()


@scenario("plugin")
def plugin_tool(ctx):
    """a plugin tool offered to the model and executed (tool.execute.before rewrites its input)."""
    boot(ctx, [{"package": "./plugins/promise-plugin", "options": {"suffix": "!"}}], ["promise-plugin"], script=[
        {"tools": [{"name": "echo", "args": {"text": "hi"}}]},
        {"text": "Echoed."},
    ])
    sid = ctx.create_session()
    ctx.prompt(sid, "echo please")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("command", "POST", f"/api/session/{sid}/command", {"name": "hello", "text": ""})
    ctx.record_events()
    ctx.record_llm()
    record_tool_definitions(ctx)
    record_storage(ctx)


@scenario("plugin")
def plugin_hooks(ctx):
    """permission.evaluate allows/denies .env reads, tool.execute.after, session.prompt/context, shell.create.before."""
    boot(ctx, ["./plugins/permission-plugin"], ["permission-plugin"], files={
        "allowed.env": "SECRET=1\n", "blocked.env": "SECRET=2\n",
    }, script=[
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/allowed.env"}}]},
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/blocked.env"}}]},
        {"tools": [{"name": "glob", "args": {"pattern": "*.env"}}]},
        {"tools": [{"name": "shell", "args": {"command": "printf %s \"$FIXTURE_SHELL\"", "description": "Print"}}]},
        {"text": "Done."},
    ])
    sid = ctx.create_session()
    ctx.prompt(sid, "read the env files")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()
    record_system(ctx)


@scenario("plugin")
def plugin_effect(ctx):
    """an Effect plugin: agent/skill transforms and a tool.execute.before hook that rejects a call."""
    boot(ctx, ["./plugins/effect-plugin"], ["effect-plugin"], script=[
        {"tools": [{"name": "forbidden", "args": {}}]},
        {"text": "Rejected."},
    ])
    sid = ctx.create_session()
    settle_lists(ctx)
    record_lists(ctx)
    ctx.prompt(sid, "try it")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("plugin", hold_for_title=0)
def plugin_rpc_storage(ctx):
    """plugin storage, ctx.event.subscribe and rpc.call (success, declared error, unknown method/RPC)."""
    boot(ctx, ["./plugins/storage-events"], ["storage-events"])
    ctx.create_session(title="Boot")
    settle_lists(ctx)
    ctx.call("plugin.list", "GET", f"/api/plugin?directory={ctx.project}")
    ctx.call("rpc.ok", "POST", "/api/rpc/fixture.echo/ping", {"input": {"text": "hey"}})
    ctx.call("rpc.refused", "POST", "/api/rpc/fixture.echo/ping", {"input": {"text": "no"}})
    ctx.call("rpc.invalid", "POST", "/api/rpc/fixture.echo/ping", {"input": {"text": 3}})
    ctx.call("rpc.method", "POST", "/api/rpc/fixture.echo/nope", {})
    ctx.call("rpc.missing", "POST", "/api/rpc/missing/ping", {})
    time.sleep(1.0)
    ctx.record_events()
    record_storage(ctx)

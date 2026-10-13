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


def boot(ctx, plugins, names, script=None, files=None, git_repo=False):
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script(script or [])
    ctx.make_project(files=dict(plugin_files(*names), **(files or {})), config_extra={"plugins": plugins},
                     git_repo=git_repo)
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


@scenario("plugin")
def plugin_http_hooks(ctx):
    """session.http.request rewrites headers and body of every model request; session.http.response rewrites the stream."""
    boot(ctx, ["./plugins/http-plugin"], ["http-plugin"], script=[
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"text": ["Say ORIGINAL", " now"]},
    ], files={"hello.txt": "hi\n"})
    sid = ctx.create_session()
    ctx.prompt(sid, "read hello")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    reqs = sorted(ctx.mock.requests, key=lambda r: r["kind"] != "title")
    out = []
    for r in reqs:
        h = {k.lower(): v for k, v in r["headers"].items()}
        body = r["body"] if isinstance(r["body"], dict) else {}
        out.append({"kind": r["kind"], "seen": h.get("x-fixture-seen"), "user": body.get("user"),
                    "trace": [k for k in ("b3", "traceparent") if k in h]})
    ctx.rec("requests", out)


@scenario("plugin", hold_for_title=0)
def plugin_vcs_worktree(ctx):
    """vcs and worktree transforms: a selected plugin VCS provider and a plugin worktree strategy."""
    boot(ctx, ["./plugins/vcs-plugin"], ["vcs-plugin"], git_repo=True)
    sid = ctx.create_session(title="Boot")
    settle_lists(ctx)
    d = f"directory={ctx.project}"
    _, session = ctx.call("session.get", "GET", f"/api/session/{sid}", record=False)
    pid = ((session or {}).get("data") or {}).get("projectID")
    # Upstream serves vcs.get from a cache its State refresh fills asynchronously.
    ctx.call("vcs.get.warmup", "GET", f"/api/vcs?{d}", record=False)
    time.sleep(1.0)
    for label, path in [
        ("vcs.get", f"/api/vcs?{d}"),
        ("vcs.base", f"/api/vcs/base?{d}"),
        ("vcs.status", f"/api/vcs/status?{d}"),
        ("vcs.branch.list", f"/api/vcs/branch?{d}&search=fe&limit=3"),
        ("vcs.diff.working", f"/api/vcs/diff?{d}&mode=working"),
        ("vcs.diff.branch", f"/api/vcs/diff?{d}&mode=branch&base=main&context=5"),
        ("vcs.diff.committed", f"/api/vcs/diff?{d}&mode=committed"),
    ]:
        ctx.call(label, "GET", path)
    ctx.call("vcs.init", "POST", f"/api/vcs/init?{d}&provider=fake", {})
    ctx.call("worktree.create", "POST", "/api/worktree", {"projectID": pid, "name": "wt1"})
    ctx.call("worktree.list", "GET", f"/api/worktree?projectID={pid}")
    ctx.call("worktree.refresh", "POST", "/api/worktree/refresh", {"projectID": pid})
    _, listed = ctx.call("worktree.list.refreshed", "GET", f"/api/worktree?projectID={pid}")
    rows = listed if isinstance(listed, list) else ((listed or {}).get("data") or [])
    created = [w["directory"] for w in rows if w.get("strategy") == "fake"]
    target = created[0] if created else ctx.project + "/missing"
    ctx.call("worktree.remove", "DELETE", "/api/worktree", {"projectID": pid, "directory": target, "force": False})
    ctx.call("worktree.remove.force", "DELETE", "/api/worktree", {"projectID": pid, "directory": target, "force": True})
    ctx.call("worktree.list.after", "GET", f"/api/worktree?projectID={pid}")


AISDK_PROVIDER = "aisdk:file://" + os.path.join(FIXTURES, "aisdk-provider", "index.js")


def aisdk_config(ctx, plugins):
    """The fixture AI SDK provider (crates/oc-plugin/tests/fixtures/aisdk-provider, a LanguageModelV3 package
    loaded from a file:// URL) as provider `fixture`, talking to the mock; plus a provider whose package is
    missing."""
    base = f"http://127.0.0.1:{ctx.mock.port}/v1"
    return {"plugins": plugins, "model": "fixture/m1", "providers": {
        "fixture": {"name": "Fixture", "package": AISDK_PROVIDER,
                    "settings": {"baseURL": base, "apiKey": "test", "marker": "configured"},
                    "models": {"m1": {"name": "Fixture One", "limit": {"context": 100000, "output": 4096}}}},
        "broken": {"name": "Broken", "package": "aisdk:file:///nonexistent/ocd-aisdk-missing.js",
                   "models": {"m1": {"name": "Broken One", "limit": {"context": 100000, "output": 4096}}}},
    }}


def record_aisdk_requests(ctx):
    """Every request the fixture provider sent (title first): path, headers (minus transport ones) and body."""
    out = []
    for r in sorted(ctx.mock.requests, key=lambda r: r["kind"] != "title"):
        h = {k.lower(): v for k, v in r["headers"].items()
             if k.lower() not in ("content-length", "host", "connection", "accept-encoding", "accept")}
        out.append({"kind": r["kind"], "path": r["path"], "headers": h, "body": r["body"]})
    ctx.rec("requests", out)


@scenario("plugin", hold_for_title=0)
def plugin_aisdk(ctx):
    """an AI SDK provider package without a native implementation (the bridge): text, reasoning, a tool call,
    usage, a provider HTTP error, a stream cut mid-way (retried) and a package that fails to load."""
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script([
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}}]},
        {"reasoning": "The file greets.", "text": ["The file ", "says hi."],
         "usage": {"prompt_tokens": 12, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 2},
                   "completion_tokens_details": {"reasoning_tokens": 3}}},
        {"status": 400, "body": {"error": {"message": "bad request from mock", "type": "invalid_request_error"}}},
        {"text": "partial", "cut": True},
        {"text": "Recovered."},
    ])
    ctx.make_project(files={"hello.txt": "hi\n"}, config_extra=aisdk_config(ctx, []))
    ctx.start()
    # No title request: the AI SDK route starts its step before the request goes out (upstream
    # `streamLanguage`), so holding the main response for the title does not order title and step.
    sid = ctx.create_session(title="Boot")
    ctx.prompt(sid, "read hello")
    ctx.prompt(sid, "again")
    ctx.prompt(sid, "cut")
    ctx.call("session.model", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "broken", "id": "m1"}})
    ctx.prompt(sid, "broken")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    record_aisdk_requests(ctx)


@scenario("plugin", hold_for_title=0)
def plugin_aisdk_hooks(ctx):
    """plugin aisdk.sdk / aisdk.language hooks (wrap the SDK and the language model) and session.http.* hooks
    around the AI SDK provider's fetch."""
    ctx.env_extra["OCD_BUN_PATH"] = BUN
    ctx.set_script([{"text": ["Say ORIGINAL", " now"]}])
    ctx.make_project(files=plugin_files("aisdk-plugin", "http-plugin"),
                     config_extra=aisdk_config(ctx, ["./plugins/aisdk-plugin", "./plugins/http-plugin"]))
    ctx.start()
    sid = ctx.create_session(title="Boot")
    ctx.prompt(sid, "hi")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    record_aisdk_requests(ctx)

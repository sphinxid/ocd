"""2.2.5 session, tool, MCP, Code Mode and terminal changes: reasoning across model switches and interrupted
responses, read/edit misses, `bash` run as `shell`, MCP stdio framing and quiet location shutdown, Code Mode `fetch`
under webfetch permissions, well-known config, renaming and removing persistent terminals, and restart recovery
only in the managed service."""
import glob
import json
import os
import shutil
import sys
import time
import urllib.parse

from parity.loop import run_prompt
from parity.registry import scenario

HERE = os.path.dirname(os.path.abspath(__file__))
SCRATCH = os.environ.get("OCD_SCRATCH", "/tmp/ocd-scratch")


def two_providers(port):
    """`mock/m1` on chat completions and `claude/m2` on the Messages API, both at the mock."""
    return {"providers": {
        "mock": {"name": "Mock", "package": "@opencode/ai/providers/openai-compatible",
                 "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
                 "models": {"m1": {"name": "Mock One", "limit": {"context": 100000, "output": 4096}},
                            "m3": {"name": "Mock Three", "limit": {"context": 100000, "output": 4096}}}},
        "claude": {"name": "Messages", "package": "@opencode/ai/providers/anthropic",
                   "settings": {"baseURL": f"http://127.0.0.1:{port}/v1", "apiKey": "test"},
                   "models": {"m2": {"name": "Messages Two", "limit": {"context": 100000, "output": 4096}}}}}}


@scenario("session225")
def session225_reasoning_switch(ctx):
    """reasoning from one model replayed to another provider and model is never turned into text (#60498)."""
    def after(ctx, sid):
        ctx.call("model.claude", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "claude", "id": "m2"}})
        ctx.prompt(sid, "and now?", label="prompt.claude")
        ctx.call("model.m3", "POST", f"/api/session/{sid}/model", {"model": {"providerID": "mock", "id": "m3"}})
        ctx.prompt(sid, "and again?", label="prompt.m3")

    run_prompt(ctx, [
        {"reasoning": ["Let me ", "think."], "text": "First answer."},
        {"text": "Second answer."},
        {"text": "Third answer."},
    ], config_extra=two_providers(ctx.mock.port), after=after, llm_full=True)


@scenario("session225")
def session225_reasoning_interrupted(ctx):
    """a stream cut after unfinished reasoning continues without replaying that reasoning (#60498)."""
    run_prompt(ctx, [
        {"reasoning": ["Partial ", "thought"], "cut": True},
        {"text": "Recovered."},
    ], llm_full=True, timeout=60)


@scenario("session225")
def session225_tool_misses(ctx):
    """read names the missing folder and its nearest parent (#60688); edit shows the closest lines or says the change
    is already there (#60689); a `bash` call runs as `shell` with a note (#60687)."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "id": "call_folder", "args": {"path": "$PROJECT/gone/sub/file.ts"}}]},
        {"tools": [{"name": "read", "id": "call_similar", "args": {"path": "$PROJECT/src/conf"}}]},
        {"tools": [{"name": "edit", "id": "call_close", "args": {
            "path": "$PROJECT/a.ts", "oldString": "export function run() {\n\tconst value = a()\n\treturn value + 2\n}",
            "newString": "x"}}]},
        {"tools": [{"name": "edit", "id": "call_applied", "args": {
            "path": "$PROJECT/a.ts", "oldString": "const value = b()",
            "newString": "  const value = a()\n  return value + 1"}}]},
        {"tools": [{"name": "bash", "id": "call_bash", "args": {"command": "echo aliased", "description": "Echo"}}]},
        {"tools": [{"name": "bash", "id": "call_bash_fail", "args": {"command": 5}}]},
        {"text": "Done."},
    ], text="misses", files={
        "a.ts": "import { a } from './a'\n\nexport function run() {\n  const value = a()\n  return value + 1\n}\n",
        "gone-old/keep.txt": "x\n",
        "src/config.json": "{}\n",
    })


def _framing_mcp(extra=None):
    server = os.path.join(HERE, "fake_mcp.py")
    config = {"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server, "--framing"]}}},
              "permission": {"execute": "deny"}}
    config.update(extra or {})
    return config


def _mcp_status(ctx, want, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/mcp?directory={ctx.project}",
                                        headers={"x-opencode-directory": ctx.project})
        if want in json.dumps(data):
            return
        time.sleep(0.2)


@scenario("session225", timeout=240)
def session225_mcp_framing(ctx):
    """an MCP result above the old 10 MiB read buffer arrives, an invalid line is only reported, and a line over
    16 MiB closes the connection (#60694). The next step still lists the failed server's tools: the session tool
    registry reads them again only 100 ms after `mcp.tools.changed` settles.

    When the connection closes, the server's status change (its own fiber) and the step that saw the call fail
    race: whether `mcp.resources.changed`/`mcp.status.changed` come before or after that step's usage is timing,
    so they are recorded on their own, in order, and left out of the session's event list."""
    mcp = lambda e: e.get("type", "").startswith("mcp.")  # noqa: E731

    def after(ctx, sid):
        ctx.call("mcp.after", "GET", f"/api/mcp?directory={ctx.project}")
        ctx.rec("events.mcp", [e for e in ctx.events() if mcp(e)])

    run_prompt(ctx, [
        {"tools": [{"name": "fake_big", "id": "call_big", "args": {}}]},
        {"tools": [{"name": "fake_huge", "id": "call_huge", "args": {}}]},
        {"text": "Done."},
    ], text="call mcp", config_extra=_framing_mcp(), before=lambda ctx, sid: _mcp_status(ctx, "connected"),
        after=after, timeout=120, events=lambda e: not mcp(e))


@scenario("session225")
def session225_location_close_quiet(ctx):
    """closing a location publishes no mcp.* events (#60668)."""
    server = os.path.join(HERE, "fake_mcp.py")
    ctx.set_script([])
    ctx.make_project(config_extra={"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server]}}}})
    ctx.start()
    ctx.create_session()
    _mcp_status(ctx, "connected")
    time.sleep(1.5)
    mark = len(ctx.server.events)
    ctx.call("debug.location.evict", "DELETE", f"/api/debug/location?directory={urllib.parse.quote(ctx.project)}")
    time.sleep(4)
    ctx.rec("events.after.evict", sorted({e.get("type") for e in ctx.server.events[mark:] if e.get("type", "").startswith("mcp.")}))


def _fetch(ctx, code, extra):
    ctx.mock.routes["/data"] = (200, {"ok": True})
    run_prompt(ctx, [
        {"tools": [{"name": "execute", "id": "call_fetch", "args": {"code": code}}]},
        {"text": "Done."},
    ], text="fetch", config_extra=_echo_mcp(extra),
        before=lambda ctx, sid: _mcp_status(ctx, "connected"))


def _echo_mcp(extra=None):
    server = os.path.join(HERE, "fake_mcp.py")
    config = {"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server, "--echo"]}}}}
    config.update(extra or {})
    return config


@scenario("session225")
def session225_codemode_fetch_allowed(ctx):
    """Code Mode fetch asks the webfetch permission for its URL; allowed, the request is sent (#59954)."""
    url = f"http://127.0.0.1:{ctx.mock.port}/data"
    _fetch(ctx, f'const r = await fetch("{url}", {{ method: "get" }}); return [r.status, await r.text()]',
           {"permission": {"webfetch": "allow"}})


@scenario("session225")
def session225_codemode_fetch_denied(ctx):
    """a URL denied for webfetch is never requested and rejects the program's promise (#59954)."""
    url = f"http://127.0.0.1:{ctx.mock.port}/data"
    _fetch(ctx, f'const [d] = await Promise.allSettled([fetch("{url}")]); return [d.status, String(d.reason?.message)]',
           {"permission": {"webfetch": "deny"}})


@scenario("session225")
def session225_wellknown_config(ctx):
    """a well-known source's config loads once its key credential exists, with the key substituted."""
    origin = f"http://127.0.0.1:{ctx.mock.port}"
    ctx.mock.routes["/.well-known/opencode"] = (200, {
        "auth": {"command": ["corp", "login"], "env": "CORP_TOKEN"},
        "config": {"username": "{env:CORP_TOKEN}"}})
    ctx.set_script([])
    ctx.make_project()
    ctx.start()
    ctx.create_session()
    ctx.call("wellknown.add", "POST", "/api/experimental/integration/wellknown", {"url": origin + "/"})
    time.sleep(1.5)
    ctx.call("config.before", "GET", "/api/config")
    ctx.call("connect", "POST", f"/api/integration/{urllib.parse.quote(origin, safe='')}/connect/key", {"key": "tok-1"})
    time.sleep(2)
    ctx.call("config.after", "GET", "/api/config")


def _pty_binary(ctx):
    """Install the 0.4.0 daemon where both binaries look for it, so nothing is downloaded."""
    found = glob.glob(os.path.join(SCRATCH, "src/opencode-2.2.5/node_modules/.bun/@opencode+pty-linux-x64-gnu@0.4.0/"
                                            "node_modules/@opencode/pty-linux-x64-gnu/bin/opencode-pty"))
    for d in (os.path.join(ctx.state, "cache", "opencode", "bin", "opencode-pty", "0.4.0-637e67684fb4fbb7"),):
        os.makedirs(d, exist_ok=True)
        if found:
            shutil.copy(found[0], os.path.join(d, "opencode-pty"))
            os.chmod(os.path.join(d, "opencode-pty"), 0o755)


@scenario("session225", hold_for_title=0)
def session225_terminal_rename_and_delete(ctx):
    """terminals: rename (userTitle, persistent-pty.updated), clear, resize only, then the session's deletion
    removes them (#60322, #60329)."""
    ctx.set_script([])
    ctx.make_project()
    _pty_binary(ctx)
    ctx.start()
    sid = ctx.create_session(title="Terminals")
    other = ctx.create_session(label="session.create.other", title="Other")
    _, created = ctx.call("terminal.create", "POST", f"/api/experimental/session/{sid}/terminal",
                          {"command": "/bin/sh", "args": [], "title": "Shell", "env": {}, "cwd": ctx.project})
    ctx.call("terminal.create.other", "POST", f"/api/experimental/session/{other}/terminal",
             {"command": "/bin/sh", "args": [], "title": "Other", "env": {}, "cwd": ctx.project})
    pty = (created or {}).get("data", created or {}).get("id", "pty_persistent_0") if isinstance(created, dict) else "x"
    base = f"/api/experimental/persistent-pty/{pty}"
    ctx.call("rename", "PUT", base, {"title": "Build"})
    ctx.call("rename.clear", "PUT", base, {"title": None})
    ctx.call("resize", "PUT", base, {"size": {"cols": 100, "rows": 30}})
    ctx.call("resize.rename", "PUT", base, {"size": {"cols": 90, "rows": 20}, "title": "Both"})
    ctx.call("rename.missing", "PUT", "/api/experimental/persistent-pty/pty_persistent_999", {"title": "x"})
    ctx.call("rename.bad", "PUT", base, {"title": 5})
    ctx.call("get", "GET", base)
    mark = len(ctx.server.events)
    ctx.call("session.delete", "DELETE", f"/api/session/{sid}")
    ctx.wait_for("persistent-pty.removed", timeout=10, start=mark)
    time.sleep(1)
    ctx.call("list.deleted", "GET", f"/api/experimental/session/{sid}/terminal")
    ctx.call("list.other", "GET", f"/api/experimental/session/{other}/terminal")
    ctx.record_events(filt=lambda e: e.get("type", "").startswith("persistent-pty."))
    ctx.call("pty.shutdown", "POST", "/api/experimental/persistent-pty/shutdown")


def _service_password(ctx):
    for path in glob.glob(os.path.join(ctx.state, "config", "opencode", "service*.json")) or [
            os.path.join(ctx.state, "config", "opencode", "service.json")]:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump({"disabled": True, "password": "parity"}, f)


@scenario("session225", hold_for_title=0, timeout=240)
def session225_restart_recovery(ctx):
    """a session whose server died mid-step is not resumed by a plain server; `serve --service` resumes it."""
    ctx.set_script([{"hang": True}, {"text": "Resumed."}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Restart")
    ctx.call("prompt", "POST", f"/api/session/{sid}/prompt", {"text": "hello"})
    deadline = time.time() + 20
    while time.time() < deadline and not ctx.mock.main_requests():
        time.sleep(0.2)
    time.sleep(1)
    ctx.server.proc.kill()
    ctx.server.proc.wait(5)
    ctx.server = None
    ctx.start()
    time.sleep(4)
    ctx.rec("plain.requests", len(ctx.mock.main_requests()))
    ctx.stop()
    _service_password(ctx)
    port = 47000 + (os.getpid() % 1000)
    ctx.start(["--service", "--port", str(port), "--hostname", "127.0.0.1"])
    deadline = time.time() + 20
    while time.time() < deadline and len(ctx.mock.main_requests()) < 2:
        time.sleep(0.2)
    time.sleep(2)
    ctx.rec("service.requests", len(ctx.mock.main_requests()))
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.stop()

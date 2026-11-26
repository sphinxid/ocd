"""2.3.1's opt-in MCP features: connection pooling (`pool: true`, 2.2.6/2.2.7) and automation event triggers (on MCP
Events, which need no flag since 2.4.1; 2.3.x's `watch_mcp_event` tool is gone, `mcp241` covers `list_mcp_events`).
The MCP servers are `fake_mcp_modern.py`, a local stdio server speaking the 2026-07-28 revision (or only the legacy
handshake with `--legacy`)."""
import json
import os
import sys
import time
import urllib.parse

from parity.registry import scenario

SERVER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_mcp_modern.py")


def _local(*flags, cwd=None, pool=True):
    config = {"type": "local", "command": [sys.executable, SERVER, *flags], "timeout": {"startup": 10000}}
    if cwd:
        config["cwd"] = cwd
    if pool:
        config["pool"] = True
    return config


def _statuses(ctx, directory):
    _, _, data = ctx.server.request("GET", f"/api/mcp?directory={urllib.parse.quote(directory)}",
                                    headers={"x-opencode-directory": directory})
    return {s["name"]: s["status"] for s in (data or {}).get("data", [])} if isinstance(data, dict) else {}


def _settled(ctx, directory, timeout=30):
    deadline = time.time() + timeout
    statuses = {}
    while time.time() < deadline:
        statuses = _statuses(ctx, directory)
        if statuses and all(s.get("status") != "pending" for s in statuses.values()):
            return statuses
        time.sleep(0.2)
    return statuses


def _lines(path):
    try:
        with open(path) as f:
            return [line.strip() for line in f if line.strip()]
    except OSError:
        return []


def _alive(pid):
    try:
        os.kill(int(pid), 0)
    except (OSError, ValueError):
        return False
    # A zombie is gone for our purposes.
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().split(")")[-1].split()[0] != "Z"
    except OSError:
        return True


def _counts(path):
    counts = {}
    for method in _lines(path):
        counts[method] = counts.get(method, 0) + 1
    return dict(sorted(counts.items()))


def _mcp_events(ctx, start):
    """The `mcp.*` events since `start`, per location and server, in order."""
    out = {}
    for e in ctx.server.events[start:]:
        kind = e.get("type", "")
        if not kind.startswith("mcp."):
            continue
        location = (e.get("location") or {}).get("directory", "")
        location = "project" if location == ctx.project else os.path.relpath(location, ctx.project) if location else "?"
        out.setdefault(f"{location} {e.get('data', {}).get('server')}", []).append(kind)
    return dict(sorted(out.items()))


@scenario("mcpopt231", timeout=150)
def mcpopt231_pool_shares_local_servers(ctx):
    """Two Locations of one server process configure the same local servers with `pool: true`: a 2026-07-28 server
    with a fixed `cwd` runs once for both, a legacy one runs privately per Location after one shared attempt, one
    without a fixed `cwd` runs per directory, and an unpooled one per Location. Disconnecting the shared server in one
    Location keeps its process for the other; disconnecting it there too stops it."""
    files = os.path.join(ctx.state, "mcp")
    fixed = os.path.join(ctx.state, "mcp-cwd")
    pids = lambda name: os.path.join(files, f"{name}.pids")  # noqa: E731
    wire = lambda name: os.path.join(files, f"{name}.wire")  # noqa: E731
    servers = {
        "shared": _local(f"--pid-file={pids('shared')}", f"--wire-log={wire('shared')}", cwd=fixed),
        "legacy": _local("--legacy", f"--pid-file={pids('legacy')}", f"--wire-log={wire('legacy')}", cwd=fixed),
        "unfixed": _local(f"--pid-file={pids('unfixed')}"),
        "plain": _local(f"--pid-file={pids('plain')}", cwd=fixed, pool=False),
    }
    ctx.set_script([])
    ctx.make_project(config_extra={"mcp": {"servers": servers}})
    os.makedirs(files, exist_ok=True)
    os.makedirs(fixed, exist_ok=True)
    other = os.path.join(ctx.project, "other")
    os.makedirs(other, exist_ok=True)
    ctx.start()
    start = len(ctx.server.events)
    ctx.create_session()
    ctx.rec("project", _settled(ctx, ctx.project))
    ctx.rec("other", _settled(ctx, other))
    time.sleep(0.5)
    ctx.rec("spawned", {name: len(_lines(pids(name))) for name in servers})
    ctx.rec("wire", {name: _counts(wire(name)) for name in ("shared", "legacy")})
    ctx.rec("events", _mcp_events(ctx, start))
    shared_pid = (_lines(pids("shared")) or ["0"])[0]

    mark = len(ctx.server.events)
    ctx.call("disconnect.project", "POST", f"/api/experimental/mcp/shared/disconnect?directory={urllib.parse.quote(ctx.project)}")
    time.sleep(1)
    ctx.rec("after.project", {"project": _statuses(ctx, ctx.project).get("shared"), "other": _statuses(ctx, other).get("shared"),
                              "alive": _alive(shared_pid)})
    ctx.call("mcp.other", "GET", f"/api/mcp?directory={urllib.parse.quote(other)}", headers={"x-opencode-directory": other})
    ctx.call("disconnect.other", "POST", f"/api/experimental/mcp/shared/disconnect?directory={urllib.parse.quote(other)}",
             headers={"x-opencode-directory": other})
    deadline = time.time() + 10
    while time.time() < deadline and _alive(shared_pid):
        time.sleep(0.1)
    ctx.rec("after.other", {"alive": _alive(shared_pid)})
    ctx.call("connect.project", "POST", f"/api/experimental/mcp/shared/connect?directory={urllib.parse.quote(ctx.project)}")
    ctx.rec("reconnected", {"project": _statuses(ctx, ctx.project).get("shared"), "spawned": len(_lines(pids("shared")))})
    ctx.rec("events.after", _mcp_events(ctx, mark))


def _emit(path, *events):
    with open(path, "a") as f:
        for event in events:
            f.write(json.dumps(event) + "\n")


def _boot_events(ctx, script, servers, extra=None):
    """Starts the server with `servers`; the fakes' files live in `<state>/mcp`."""
    ctx.set_script(script)
    config = {"mcp": {"servers": servers}, "permission": {"*": "allow", "execute": "deny"}}
    config.update(extra or {})
    ctx.make_project(config_extra=config)
    os.makedirs(os.path.join(ctx.state, "mcp"), exist_ok=True)
    ctx.start()


def _wire(path):
    """The fake's requests, without the handshake's (which differ in nothing the scenario checks)."""
    return [m for m in _lines(path) if m not in ("ping",)]


def _automation_rows(ctx):
    """The automation table's event bookkeeping, read from the data directory's database."""
    import glob
    import sqlite3
    rows = []
    for path in sorted(glob.glob(os.path.join(ctx.state, "data", "opencode", "*.db"))):
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        try:
            for name, status, run_count, outcome, position, due in con.execute(
                    "SELECT name, status, run_count, last_outcome, event_position, next_due_at FROM automation ORDER BY time_created"):
                rows.append({"name": name, "status": status, "runCount": run_count, "lastOutcome": outcome,
                             "eventPosition": json.loads(position) if position else None, "nextDueAt": due})
        except sqlite3.Error as error:
            rows.append({"error": str(error)})
        finally:
            con.close()
    return rows


@scenario("mcpopt231", hold_for_title=0, timeout=180)
def mcpopt231_automation_event_fires(ctx):
    """An automation with an `event` trigger (`automations: true`), created by the model with
    `automation_create`: the watch's stream opens with the trigger's arguments, the server's event fires the
    prompt into the session with the event quoted as untrusted data, a heartbeat and the event move `event_position`,
    and an event the payload schema refuses is dropped by the MCP client, so the watch stays active (2.4.1)."""
    files = os.path.join(ctx.state, "mcp")
    emit, wire = os.path.join(files, "emit"), os.path.join(files, "wire")
    servers = {"jobs": {"type": "local", "protocol": "2026-07-28",
                        "command": [sys.executable, SERVER, "--events", f"--emit={emit}", f"--wire-log={wire}"]}}
    create = {"name": "On job", "trigger": {"type": "event", "server": "jobs", "event": "job.finished", "args": {"job": "j1"}},
              "action": {"type": "prompt", "prompt": "Report the finished job."}}
    _boot_events(ctx, [
        {"tools": [{"name": "automation_create", "id": "call_1",
                    "args": {**create, "trigger": {**create["trigger"], "event": "poll.only"}}}]},
        {"tools": [{"name": "automation_create", "id": "call_2",
                    "args": {**create, "trigger": {**create["trigger"], "args": {"job": 1}}}}]},
        {"tools": [{"name": "automation_create", "id": "call_3", "args": create}]},
        {"text": "Watching."},
        {"text": "The job finished."},
    ], servers, extra={"automations": True})
    sid = ctx.create_session(title="Automation")
    _settled(ctx, ctx.project)
    ctx.prompt(sid, "when job j1 finishes, report it", timeout=90)
    deadline = time.time() + 20
    while time.time() < deadline and _lines(wire).count("events/stream") < 1:
        time.sleep(0.1)
    ctx.rec("rows.created", _automation_rows(ctx))
    mark = len(ctx.server.events)
    _emit(emit, {"eventId": "evt-1", "data": {"status": "done"}, "cursor": "c1"})
    ctx.wait_for("session.inbox.enqueued", timeout=30, start=mark)
    ctx.wait_idle(sid, start=mark, timeout=60)
    time.sleep(0.5)
    ctx.rec("rows.fired", _automation_rows(ctx))
    _emit(emit, {"eventId": "evt-2", "data": {"status": 2}, "cursor": "c2"})
    time.sleep(2)
    ctx.rec("rows.dropped", _automation_rows(ctx))
    ctx.call("automations", "GET", "/api/experimental/automation")
    ctx.rec("wire", _wire(wire))
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: not e.get("type", "").startswith("mcp."))
    ctx.record_llm(full=True)

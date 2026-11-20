"""2.3.1's opt-in MCP features: connection pooling (`pool: true`, 2.2.6/2.2.7), MCP Events (`experimental.mcp_events`)
with the `watch_mcp_event` tool, and automation event triggers. The MCP servers are `fake_mcp_modern.py`, a local
stdio server speaking the 2026-07-28 revision (or only the legacy handshake with `--legacy`)."""
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

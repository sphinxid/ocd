"""2.4.1's MCP changes: an unset `protocol` negotiates `auto` (#65282) and `list_mcp_events` replaces
`watch_mcp_event` (#64586), always registered now that MCP Events need no flag (#65271). The MCP servers are
`fake_mcp_modern.py`, a local stdio server speaking the 2026-07-28 revision (or only the legacy handshake with
`--legacy`)."""
import os
import sys
import time

from parity.mcpopt231 import SERVER, _counts, _lines, _settled
from parity.registry import scenario


def _local(*flags, protocol=None):
    config = {"type": "local", "command": [sys.executable, SERVER, *flags], "timeout": {"startup": 10000}}
    if protocol:
        config["protocol"] = protocol
    return config


@scenario("mcp241", timeout=150)
def mcp241_unset_protocol_negotiates_auto(ctx):
    """Unpooled local servers with no `protocol`: a 2026-07-28 server answers the probe; a legacy one refuses it and
    initializes on the same process; one that exits on the probe is spawned again for the legacy handshake; one that
    never answers it initializes in place once the probe gives up (its 5 s, or the shorter startup timeout); an
    explicit `legacy` skips the probe."""
    files = os.path.join(ctx.state, "mcp")
    pids = lambda name: os.path.join(files, f"{name}.pids")  # noqa: E731
    wire = lambda name: os.path.join(files, f"{name}.wire")  # noqa: E731
    servers = {
        "modern": _local(f"--pid-file={pids('modern')}", f"--wire-log={wire('modern')}"),
        "legacy": _local("--legacy", f"--pid-file={pids('legacy')}", f"--wire-log={wire('legacy')}"),
        "exits": _local("--legacy", "--exit-on-discover", f"--pid-file={pids('exits')}", f"--wire-log={wire('exits')}"),
        "silent": {**_local("--legacy", "--silent-discover", f"--pid-file={pids('silent')}", f"--wire-log={wire('silent')}"),
                   "timeout": {"startup": 2000}},
        "pinned": _local("--legacy", f"--pid-file={pids('pinned')}", f"--wire-log={wire('pinned')}", protocol="legacy"),
    }
    ctx.set_script([])
    ctx.make_project(config_extra={"mcp": {"servers": servers}})
    os.makedirs(files, exist_ok=True)
    ctx.start()
    ctx.create_session()
    ctx.rec("statuses", _settled(ctx, ctx.project))
    time.sleep(0.5)
    ctx.rec("spawned", {name: len(_lines(pids(name))) for name in servers})
    ctx.rec("wire", {name: _counts(wire(name)) for name in servers})
    ctx.rec("first", {name: (_lines(wire(name)) or [None])[0] for name in servers})


@scenario("mcp241", hold_for_title=0, timeout=150)
def mcp241_list_mcp_events(ctx):
    """`opencode_list_mcp_events` (Code Mode off), offered with no flag: the model lists one server's push event types,
    shaped like the event trigger; a server connected over the legacy handshake, where MCP Events aren't defined,
    sends none. The tool's definition is in the recorded model requests."""
    files = os.path.join(ctx.state, "mcp")
    params = os.path.join(files, "params")
    servers = {
        "jobs": _local("--events", f"--params-log={params}"),
        "old": _local("--events", protocol="legacy"),
    }
    ctx.set_script([
        {"tools": [{"name": "opencode_list_mcp_events", "id": "call_1", "args": {"server": "jobs"}}]},
        {"tools": [{"name": "opencode_list_mcp_events", "id": "call_2", "args": {"server": "old"}}]},
        {"text": "Listed."},
    ])
    # Automations and media generation are other batches' (their tools are left out so the requests compare).
    ctx.make_project(config_extra={"mcp": {"servers": servers}, "permission": {"*": "allow", "execute": "deny"},
                                   "automations": False,
                                   "experimental": {"image_generate": False, "video_generate": False}})
    os.makedirs(files, exist_ok=True)
    ctx.start()
    sid = ctx.create_session(title="Events")
    _settled(ctx, ctx.project)
    ctx.prompt(sid, "what events does jobs send?", timeout=90)
    ctx.rec("params", _lines(params))
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: not e.get("type", "").startswith("mcp."))
    ctx.record_llm(full=True)


@scenario("mcp241", hold_for_title=0, timeout=180)
def mcp241_event_watch_wire(ctx):
    """An event automation created through Code Mode (`tools.automation.create`, `automations: true`): the MCP Events
    beta on the wire (`events/list`, then `events/stream` with the trigger's arguments), an event firing the prompt,
    a heartbeat and the event moving `event_position`, and an event the payload schema refuses dropped by the client
    while the watch stays active. Compares fully once ocd has 2.4.1's automation namespace."""
    from parity.codemode230 import execute
    from parity.mcpopt231 import _automation_rows, _emit, _wire
    files = os.path.join(ctx.state, "mcp")
    emit, wire, params = (os.path.join(files, name) for name in ("emit", "wire", "params"))
    servers = {"jobs": _local("--events", f"--emit={emit}", f"--wire-log={wire}", f"--params-log={params}")}
    code = ('return await tools.automation.create({ name: "On job", trigger: { type: "event", server: "jobs", '
            'event: "job.finished", args: { job: "j1" } }, action: { type: "prompt", prompt: "Report the finished job." } })')
    ctx.set_script([
        {"tools": [execute(code, "call_1")]},
        {"text": "Watching."},
        {"text": "The job finished."},
    ])
    ctx.make_project(config_extra={"mcp": {"servers": servers}, "permission": {"*": "allow"}, "automations": True})
    os.makedirs(files, exist_ok=True)
    ctx.start()
    sid = ctx.create_session(title="Automation")
    _settled(ctx, ctx.project)
    ctx.prompt(sid, "when job j1 finishes, report it", timeout=90)
    deadline = time.time() + 20
    while time.time() < deadline and "events/stream" not in _lines(wire):
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
    ctx.rec("params", _lines(params))
    ctx.rec("wire", _wire(wire))

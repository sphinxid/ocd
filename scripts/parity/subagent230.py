"""Subagents in 2.3.0 (`core/session/execution/subagent-report.ts`, `job.ts`, `tool/plugin/subagent.ts`): a subagent
always runs in the background (the tool returns at once, with no `background` input), and the parent gets a
`<subagent>` report every time the child goes idle: after its first run, after each later run started over HTTP or by
the child's own background work, for a child created directly over HTTP, and when the child is stopped. A report sent
while the child still has background work lists it ("Background work still running:") and is tagged
`state="running"`. Job rows are written when the job starts and updated in place as the report's outbox.

The parent's requests and the child's race each other, so the requests are recorded per session, each list in order;
durable events are compared per aggregate in `seq` order. A restart with a child in flight runs `serve --service`
twice on a free port (only the managed service resumes claimed sessions)."""
import json
import os
import signal
import socket
import time

from parity.harness import PASSWORD, Server
from parity.registry import scenario


def launch(match, prompt, description="Review notes", call="call_launch"):
    return {"match": match, "tools": [{"name": "subagent", "id": call, "args": {
        "agent": "general", "description": description, "prompt": prompt}}]}


def no_deltas(e):
    return not e.get("type", "").endswith(".delta")


def record_llm(ctx, label="llm"):
    """The requests of each session, in order: the parent's and the children's race each other."""
    groups = {}
    for r in ctx.mock.main_requests():
        msgs = [m for m in r["body"].get("messages", []) if m.get("role") != "system"]
        first = str((msgs[0] if msgs else {}).get("content"))
        key = "child" if first.startswith("You are a subagent") or "DIRECT-" in first else "parent"
        tools = sorted(t.get("function", {}).get("name", "?") for t in r["body"].get("tools", []) or [])
        groups.setdefault(key, []).append({"messages": msgs, "tools": tools})
    for key in ("parent", "child"):
        ctx.rec(f"{label}.{key}", groups.get(key, []))


def child_of(ctx, sid):
    for e in ctx.events():
        d = e.get("data") or {}
        if e.get("type") == "session.created" and d.get("parentID") == sid:
            return d["sessionID"]
    return None


def reports(ctx, sid):
    """The `<subagent>` notices delivered to the parent so far (unrecorded reads)."""
    _, _, data = ctx.server.request("GET", f"/api/session/{sid}/message", headers={"x-opencode-directory": ctx.project})
    items = data.get("data", data) if isinstance(data, dict) else data
    return [m for m in items or [] if isinstance(m, dict) and m.get("type") == "synthetic" and '<background-result kind="subagent" ' in str(m.get("text"))]


def wait_reports(ctx, sid, count, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if len(reports(ctx, sid)) >= count:
            break
        time.sleep(0.1)
    else:
        ctx.rec("TIMEOUT waiting for reports", {"session": sid, "count": count})
    # The report wakes the parent; wait for its answer.
    quiet(ctx)


def quiet(ctx, gap=1.5, timeout=30):
    """Until no event arrived for `gap` seconds."""
    deadline = time.time() + timeout
    count = len(ctx.server.events)
    while time.time() < deadline:
        time.sleep(gap)
        if len(ctx.server.events) == count:
            return
        count = len(ctx.server.events)


def record(ctx, parent, child, events=True):
    ctx.call("parent.messages", "GET", f"/api/session/{parent}/message")
    if child:
        ctx.call("child.messages", "GET", f"/api/session/{child}/message")
    ctx.call("jobs", "GET", "/api/experimental/job")
    if events:
        ctx.record_events(filt=no_deltas)
    record_llm(ctx)


@scenario("subagent")
def subagent_report_per_idle(ctx):
    """the tool returns the running handle at once; the parent gets a report when the child goes idle, and another
    after a follow-up prompted to the child over HTTP."""
    ctx.set_script([
        launch("start the review", "CHILD-R: review the notes"),
        {"match": "working in the background", "text": "Launched the review."},
        {"match": "CHILD-R: review", "delay": 1.0, "text": "First review: two notes."},
        {"match": "First review: two notes", "text": "Noted the first review."},
        {"match": "CHILD-R2: check again", "text": "Second review: all good."},
        {"match": "Second review: all good", "text": "Noted the second review."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Subagent reports")
    ctx.prompt(sid, "start the review")
    wait_reports(ctx, sid, 1)
    child = child_of(ctx, sid)
    ctx.call("jobs.after_first", "GET", "/api/experimental/job")
    ctx.prompt(child, "CHILD-R2: check again", label="child.prompt")
    wait_reports(ctx, sid, 2)
    record(ctx, sid, child)
    # The tool as the model sees it: 2.3.0's description and input schema (no `background`).
    first = ctx.mock.main_requests()[0]["body"]
    ctx.rec("llm.subagent_tool", [t for t in first.get("tools", []) if t.get("function", {}).get("name") == "subagent"])


@scenario("subagent")
def subagent_report_running_work(ctx):
    """the child starts a background shell and ends its turn: the report goes out at once, lists the running shell
    and is tagged `state="running"`; the shell's notice wakes the child, which reports again when done."""
    ctx.set_script([
        launch("start the tests", "CHILD-T: run the tests", description="Run tests"),
        {"match": "working in the background", "text": "Launched the tests."},
        {"match": "CHILD-T: run the tests", "tools": [{"name": "shell", "id": "call_shell", "args": {
            "command": "sleep 15; echo shell-done", "background": True, "description": "Run tests"}}]},
        {"match": "CHILD-T", "text": "Tests started."},
        {"match": "Tests started.", "text": "Waiting for the tests."},
        {"match": "shell-done", "text": "Tests passed."},
        {"match": "Tests passed.", "text": "All done."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Running work")
    ctx.prompt(sid, "start the tests")
    wait_reports(ctx, sid, 1)
    child = child_of(ctx, sid)
    ctx.call("jobs.while_running", "GET", "/api/experimental/job")
    wait_reports(ctx, sid, 2, timeout=30)
    record(ctx, sid, child)


@scenario("subagent")
def subagent_direct_child(ctx):
    """a child session created over HTTP with a `parentID` and prompted directly reports to its parent, which wakes
    and answers."""
    ctx.set_script([
        {"match": "DIRECT-1: count the files", "text": "Direct answer: three files."},
        {"match": "Direct answer: three files", "text": "Parent saw the direct report."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Direct parent")
    child = ctx.create_session(label="child.create", parentID=sid, title="Direct child")
    ctx.prompt(child, "DIRECT-1: count the files", label="child.prompt")
    wait_reports(ctx, sid, 1)
    record(ctx, sid, child)


@scenario("subagent")
def subagent_stopped_child(ctx):
    """stopping a running child reports "Subagent cancelled" to the parent and drops the job row."""
    ctx.set_script([
        launch("start the slow work", "CHILD-S: take your time", description="Slow work"),
        {"match": "working in the background", "text": "Launched the slow work."},
        {"match": "CHILD-S: take your time", "hang": True},
        {"match": "Subagent cancelled", "text": "The slow work was stopped."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Stopped child")
    ctx.prompt(sid, "start the slow work")
    deadline = time.time() + 30
    while not ctx.mock.used[2] and time.time() < deadline:
        time.sleep(0.05)
    time.sleep(0.5)
    child = child_of(ctx, sid)
    ctx.call("jobs.before_stop", "GET", "/api/experimental/job")
    ctx.call("child.interrupt", "POST", f"/api/session/{child}/interrupt")
    wait_reports(ctx, sid, 1)
    record(ctx, sid, child)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def service_env(ctx):
    """The isolated environment disables the service for every channel; this scenario runs it on its own port, with
    the harness's password (a service takes its password from its config, not from `OPENCODE_PASSWORD`)."""
    env = ctx.env()
    service_dir = os.path.join(env["XDG_CONFIG_HOME"], "opencode")
    for name in os.listdir(service_dir):
        if name.startswith("service") and name.endswith(".json"):
            with open(os.path.join(service_dir, name), "w") as f:
                json.dump({"password": PASSWORD}, f)
    return env


@scenario("subagent")
def subagent_restart_child_in_flight(ctx):
    """the service is killed while a background child is mid-turn: the next service counts the child's restart
    attempt, resumes it with the restart notice, and its report reaches the parent exactly once, with the job row's
    notification ID."""
    ctx.set_script([
        launch("start the audit", "CHILD-X: audit the notes", description="Audit notes"),
        {"match": "working in the background", "text": "Launched the audit."},
        {"match": "CHILD-X: audit the notes", "hang": True},
        {"match": "The server restarted while you were working", "text": "Audit finished after the restart."},
        {"match": "Audit finished after the restart", "text": "Noted the audit."},
    ])
    ctx.make_project()
    port = str(free_port())
    ctx.server = Server(ctx.binary, ctx.project, service_env(ctx), ctx.name, ["--service", "--port", port])
    ctx.server_url = ctx.server.url
    ctx.event_start = 0
    sid = ctx.create_session(title="Restart parent")
    ctx.prompt(sid, "start the audit")
    deadline = time.time() + 30
    while not ctx.mock.used[2] and time.time() < deadline:
        time.sleep(0.05)
    time.sleep(0.5)
    child = child_of(ctx, sid)
    ctx.call("jobs.before_kill", "GET", "/api/experimental/job")
    ctx.rec("first.events", [e.get("type") for e in ctx.events() if no_deltas(e) and (e.get("data") or {}).get("sessionID") == child])
    ctx.server.proc.send_signal(signal.SIGKILL)
    ctx.server.proc.wait(10)
    ctx.server = Server(ctx.binary, ctx.project, service_env(ctx), ctx.name, ["--service", "--port", port])
    ctx.server_url = ctx.server.url
    ctx.event_start = 0
    wait_reports(ctx, sid, 1)
    # The second service resumes the child while it boots, before an event stream can connect: how much of that the
    # stream sees is a race, so the stored messages (and the requests) are compared instead.
    record(ctx, sid, child, events=False)


@scenario("subagent", hold_for_title=0)
def subagent_run_waits_for_report(ctx):
    """`run` waits for the subagent's job, then for the parent turn its report starts, before exiting (both modes)."""
    ctx.make_project()
    for label, args in (("run", []), ("run --format json", ["--format", "json"])):
        ctx.set_script([
            launch("start the work", "CHILD-W: do the work", description="Do work"),
            {"match": "working in the background", "text": "Launched the work."},
            {"match": "CHILD-W: do the work", "delay": 1.0, "text": "Child result: done."},
            {"match": "Child result: done.", "text": "Report handled."},
        ])
        ctx.cli(label, ["run", "--standalone", "-m", "mock/m1", *args, "start the work"], timeout=90)
        record_llm(ctx, label + ".llm")

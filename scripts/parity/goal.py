"""`/goal` scenarios (2.2): the `goal` command, the `opencode.goal` RPC, `rpc.opencode.goal.changed` and the reviewer
loop. Reviewer requests end with the judge prompt; every other request is ordinary assistant work."""
from parity.registry import scenario

REVIEW = "You are now the reviewer for the session's goal"


def review(verdict):
    return {"match": REVIEW, "text": verdict, "usage": {"prompt_tokens": 40, "completion_tokens": 8}}


def work(text="Working on it"):
    return {"text": text, "usage": {"prompt_tokens": 20, "completion_tokens": 4}}


def goal(ctx, sid, text, label="goal", wait=True, timeout=60):
    mark = len(ctx.server.events)
    status, _ = ctx.call(label, "POST", f"/api/session/{sid}/command", {"name": "goal", "text": text})
    if wait and status < 300:
        ctx.wait_idle(sid, start=mark, timeout=timeout)
    return status


def rpc(ctx, label, sid=None, method="get", body=None):
    payload = body if body is not None else {"input": {"sessionID": sid}}
    return ctx.call(label, "POST", f"/api/rpc/opencode.goal/{method}", payload)


def finish(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.call("session.get", "GET", f"/api/session/{sid}")
    ctx.record_events()
    ctx.record_llm()


@scenario("goal")
def goal_reviewer_loop(ctx):
    """/goal on an untitled session: kickoff notice, a `continue` round, then `done` in one execution."""
    ctx.set_script([
        work(),
        review('{"verdict":"continue","reason":"The parser test still fails.","next_step":"Fix the date parser."}'),
        work("Fixed the parser."),
        review('```json\n{"verdict":"done","reason":"All 12 tests pass."}\n```'),
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session()
    rpc(ctx, "rpc.before", sid)
    goal(ctx, sid, "  Make the test suite pass  ")
    rpc(ctx, "rpc.after", sid)
    finish(ctx, sid)


@scenario("goal")
def goal_controls(ctx):
    """argument handling and errors, a reviewer `paused` verdict, pause/resume/clear, RPC errors."""
    ctx.set_script([
        work(),
        review('{"verdict":"paused","reason":"The user asked to stop for now."}'),
        work("Resumed."),
        review('{"verdict":"done","reason":"The changelog lists every PR."}'),
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Goal session")
    goal(ctx, sid, "", label="goal.empty")
    goal(ctx, sid, "pause", label="goal.pause.none")
    goal(ctx, sid, "resume", label="goal.resume.none")
    goal(ctx, sid, "clear", label="goal.clear.none")
    goal(ctx, sid, "x" * 4001, label="goal.too-long")
    rpc(ctx, "rpc.missing-input", body={"input": {}})
    rpc(ctx, "rpc.bad-input", body={"input": {"sessionID": 1}})
    rpc(ctx, "rpc.no-input", body={})
    rpc(ctx, "rpc.unknown-method", sid, method="set")
    goal(ctx, sid, "'Update the changelog'")
    rpc(ctx, "rpc.paused", sid)
    goal(ctx, sid, "", label="goal.describe")
    goal(ctx, sid, "pause", label="goal.pause.paused")
    goal(ctx, sid, "Resume", label="goal.resume")
    rpc(ctx, "rpc.complete", sid)
    goal(ctx, sid, "", label="goal.describe.complete")
    goal(ctx, sid, "clear", label="goal.clear", wait=False)
    rpc(ctx, "rpc.cleared", sid)
    goal(ctx, sid, "clear", label="goal.clear.again")
    finish(ctx, sid)


@scenario("goal")
def goal_no_verdict(ctx):
    """a reviewer that never answers with a verdict: asked twice, then the goal pauses."""
    ctx.set_script([work(), review("Looks good to me."), review("I think so.")])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Goal session")
    goal(ctx, sid, "Ship it")
    rpc(ctx, "rpc", sid)
    finish(ctx, sid)


@scenario("goal")
def goal_pause_command(ctx):
    """/goal pause while the model streams: the idle hook finds the goal paused and asks no reviewer."""
    ctx.set_script([
        {"text": ["Working", " on it"], "stall": 4.0},
        review('{"verdict":"continue","reason":"More to do."}'),
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Goal session")
    mark = len(ctx.server.events)
    goal(ctx, sid, "Rename the module", wait=False)
    ctx.wait_for("session.text.delta", start=mark)
    goal(ctx, sid, "pause", label="goal.pause", wait=False)
    ctx.wait_idle(sid, start=mark)
    rpc(ctx, "rpc", sid)
    finish(ctx, sid)


@scenario("goal")
def goal_interrupt(ctx):
    """interrupting the session pauses an active goal ("Stopped by the user")."""
    ctx.set_script([{"text": "Working", "stall": 30.0}])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session(title="Goal session")
    mark = len(ctx.server.events)
    goal(ctx, sid, "Refactor the store", wait=False)
    ctx.wait_for("session.text.delta", start=mark)
    ctx.call("interrupt", "POST", f"/api/session/{sid}/interrupt", {})
    ctx.wait_idle(sid, start=mark)
    ctx.wait_for("rpc.opencode.goal.changed", start=mark,
                 pred=lambda e: ((e.get("data") or {}).get("goal") or {}).get("status") == "paused")
    rpc(ctx, "rpc", sid)
    finish(ctx, sid)


@scenario("goal")
def goal_inventory(ctx):
    """the built-in plugin list (opencode.goal and the 2.2 tool plugins) and the `goal` command."""
    ctx.make_project()
    ctx.start()
    ctx.booted()
    ctx.call("plugin.list", "GET", "/api/plugin")
    ctx.call("command.list", "GET", "/api/command")

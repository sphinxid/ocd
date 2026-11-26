"""opencode 2.4.1 session and tool changes the model sees: the `<background-result>` line of the system prompt and
the block background work reports in (#64971), misplaced tool calls run under their tool with a `<tool-repair>` note
first (#65494), small fields before large output in structured tool results (#65513), a command the shell scanner
can't analyze asking for approval (#65269), and the job cancel route with `experimental.job.updated` (#64790)."""
import time

from parity.loop import run_prompt
from parity.registry import scenario


def call(name, args, id):
    return {"name": name, "args": args, "id": id}


@scenario("session241")
def session241_tool_repair(ctx):
    """`Bash` runs as `shell` and a top-level call to the Code Mode `opencode.models` tool runs inside execute, each
    result starting with its `<tool-repair>` note; the system prompt carries the `<background-result>` line."""
    run_prompt(ctx, [
        {"tools": [call("Bash", {"command": "echo repaired"}, "call_bash")]},
        {"tools": [call("opencode.models", {"limit": 1}, "call_models")]},
        {"text": "Done."},
    ], config_extra={"permission": {"shell": "allow"}}, llm_full=True)


@scenario("session241")
def session241_structured_order(ctx):
    """A Code Mode program returns a shell result and a models page: small fields come before the large output."""
    code = ("const shell = await tools.shell({ command: 'echo ordered' });\n"
            "const models = await tools.opencode.models({ limit: 1 });\n"
            "return { shell: Object.keys(shell), models: Object.keys(models) }")
    run_prompt(ctx, [
        {"tools": [call("execute", {"code": code}, "call_exec")]},
        {"text": "Done."},
    ], config_extra={"permission": {"shell": "allow"}, "experimental": {"full_codemode": True}})


@scenario("session241")
def session241_unanalyzable_shell(ctx):
    """A shell command the scanner can't analyze asks for `external_directory` `*` and the whole command; with a
    shell deny rule configured it fails instead."""
    def approve(ctx, sid):
        mark = len(ctx.server.events)
        ctx.prompt(sid, "run it", wait=False)
        for _ in range(2):
            ev = ctx.wait_for("permission.asked", start=mark, timeout=30)
            mark = len(ctx.server.events)
            ctx.call("permission.reply", "POST",
                     f"/api/session/{sid}/permission/{ev['data']['id']}/reply", {"decision": "once"})
        ctx.wait_idle(sid)

    ctx.set_script([
        {"tools": [call("shell", {"command": "alias p=true; printf hello"}, "call_eval")]},
        {"text": "Ran it."},
        {"tools": [call("shell", {"command": "alias p=true; printf hello"}, "call_denied")]},
        {"text": "Refused."},
    ])
    ctx.make_project()
    ctx.start()
    sid = ctx.create_session()
    approve(ctx, sid)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events("asked", filt=lambda e: e.get("type") == "permission.asked")
    ctx.stop()
    ctx.make_project(config_extra={"permission": {"shell": {"*": "allow", "rm *": "deny"}, "external_directory": "allow"}})
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "run it again")
    ctx.call("messages.denied", "GET", f"/api/session/{sid}/message")
    ctx.record_llm()


@scenario("session241", hold_for_title=0, timeout=120)
def session241_background_result(ctx):
    """A background shell command reports in a `<background-result kind="shell">` block that resumes the session."""
    ctx.set_script([
        {"tools": [call("shell", {"command": "echo bg-done", "background": True}, "call_bg")]},
        {"text": "Started it."},
        {"text": "Noted."},
    ])
    ctx.make_project(config_extra={"permission": {"shell": "allow"}})
    ctx.start()
    sid = ctx.create_session(title="Background")
    ctx.prompt(sid, "start it")
    deadline = time.time() + 30
    while time.time() < deadline and len(ctx.mock.main_requests()) < 3:
        time.sleep(0.2)
    ctx.wait_idle(sid)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=lambda e: e.get("type") == "experimental.job.updated")
    ctx.record_llm()


@scenario("session241", hold_for_title=0, timeout=120)
def session241_job_cancel(ctx):
    """`POST /api/experimental/job/{id}/cancel` answers 204 for an unknown job and cancels a running background
    shell, which `experimental.job.updated` reports."""
    ctx.set_script([
        {"tools": [call("shell", {"command": "sleep 30", "background": True}, "call_bg")]},
        {"text": "Started it."},
        {"text": "Cancelled."},
    ])
    ctx.make_project(config_extra={"permission": {"shell": "allow"}})
    ctx.start()
    ctx.call("cancel.unknown", "POST", "/api/experimental/job/job_missing/cancel")
    sid = ctx.create_session(title="Cancel")
    ctx.prompt(sid, "start it")
    jobs = ctx.data("jobs", "GET", "/api/experimental/job")
    job = next(j for j in jobs if j.get("kind") == "shell")
    ctx.call("cancel", "POST", f"/api/experimental/job/{job['id']}/cancel")
    ctx.wait_for("experimental.job.updated", pred=lambda e: e["data"]["info"]["status"] == "cancelled")
    ctx.call("cancel.again", "POST", f"/api/experimental/job/{job['id']}/cancel")
    ctx.call("jobs.after", "GET", "/api/experimental/job")
    ctx.record_events(filt=lambda e: e.get("type") == "experimental.job.updated")

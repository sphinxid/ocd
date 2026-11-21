"""opencode 2.3.1 `full_codemode` (opt-in only, by `experimental.full_codemode` or `OPENCODE_FLAGS=full_codemode`):
shell, read, edit, write, patch and websearch become Code Mode tools (the first five pinned), grep, glob and webfetch
are hidden, `tools.shell` has no `background` input and a 10-minute default timeout, and a script still running after
10 s (or moved by a session-wide background request) becomes a `codemode` job whose result arrives as a
`<codemode>` synthetic message."""
from parity.codemode230 import execute
from parity.loop import run_prompt
from parity.registry import scenario

FLAG = {"experimental": {"full_codemode": True}}

# Progress rows (throttled) and parallel shells' events race the clock.
RACY = ("session.tool.progress", "shell.created", "shell.exited")


def QUIET(event):
    kind = event.get("type", "")
    return kind not in RACY and not kind.endswith(".delta")

# The fields of a shell result that do not depend on timing, and its keys in order.
SHELL = "\n".join([
    "const pick = (r) => ({ keys: Object.keys(r), output: r.output, exitCode: r.exitCode })",
    "const [one, two] = await Promise.all([tools.shell({ command: \"echo one\" }), tools.shell({ command: \"echo two\" })])",
    "return [pick(one), pick(two)]",
])


# Node's `path`, resolving from the session's directory (the other flavour from its own root).
PATH = "\n".join([
    "import { join } from \"node:path\"",
    "return [typeof path, path.resolve(\"a\", \"b\"), path.relative(\"a\", \"c\"), path.join(\"out\", \"..\", \"x.txt\"),",
    "  path.basename(\"/a/b.ts\", \".ts\"), path.posix.join(\"a\", \"b\"), path.win32.join(\"a\", \"b\"), path.win32.resolve(\"a\"),",
    "  path.sep, path.delimiter, path.parse(\"/x/y.tar.gz\"), path.matchesGlob(\"src/a.ts\", \"src/*.ts\"),",
    "  join(\"a\", \"b\"), require(\"path\").posix.dirname(\"a/b\"), Object.keys(path).length]",
])


@scenario("fcm231")
def fcm231_tools(ctx):
    """with experimental.full_codemode: the tool definitions and Code Mode catalog the model sees, two parallel
    shells, a read, a write and an edit through Code Mode, a `background` input (ignored: the command runs in the
    foreground), and Node's `path`."""
    edit = "\n".join([
        "await tools.write({ path: \"note.txt\", content: \"alpha\\n\" })",
        "await tools.edit({ path: \"note.txt\", oldString: \"alpha\", newString: \"beta\" })",
        "return await tools.read({ path: \"note.txt\" })",
    ])
    background = "\n".join([
        "try { const r = await tools.shell({ command: \"echo hi\", background: true }); return [Object.keys(r), r.output] }",
        "catch (e) { return [e.name, e.message] }",
    ])
    run_prompt(ctx, [
        {"tools": [execute(SHELL, "call_shells")]},
        {"tools": [execute("return await tools.read({ path: \"hello.txt\" })", "call_read")]},
        {"tools": [execute(edit, "call_edit")]},
        {"tools": [execute(background, "call_background")]},
        {"tools": [execute(PATH, "call_path")]},
        {"text": "Done."},
    ], text="use the tools", files={"hello.txt": "hi there\n"}, config_extra=FLAG, llm_full=True, events=QUIET)


@scenario("fcm231")
def fcm231_env_flag(ctx):
    """OPENCODE_FLAGS=full_codemode turns it on like the config key: read is reached through Code Mode only."""
    ctx.env_extra["OPENCODE_FLAGS"] = "full_codemode"
    run_prompt(ctx, [
        {"tools": [execute("return await tools.read({ path: \"hello.txt\" })", "call_read")]},
        {"text": "Read."},
    ], text="read it", files={"hello.txt": "hi there\n"}, events=QUIET)


@scenario("fcm231")
def fcm231_all_flags(ctx):
    """experimental.all_flags turns on every flag except the opt-in-only full_codemode: the tools stay direct and
    programs have no `path`."""
    run_prompt(ctx, [
        {"tools": [{"name": "read", "args": {"path": "$PROJECT/hello.txt"}, "id": "call_read"}]},
        {"tools": [execute("return [typeof path]", "call_path")]},
        {"text": "Read."},
    ], text="read it", files={"hello.txt": "hi there\n"},
        config_extra={"experimental": {"all_flags": True, "automations": False, "artifacts": False,
                                       "console_watches": False, "mcp_events": False, "media_blob": False}})


def _script(ctx, sid, mark):
    """The job list while the script runs in the background, then the session's second idle (the result step)."""
    ctx.call("job.list", "GET", "/api/experimental/job")
    ev = ctx.wait_for("session.inbox.enqueued", start=mark, timeout=60,
                      pred=lambda e: "<codemode" in str(e.get("data", {})))
    if ev is None:
        ctx.rec("TIMEOUT waiting for the <codemode> message", sid)
    ctx.wait_idle(sid, start=ctx.server.events.index(ev) if ev else None, timeout=60)


@scenario("fcm231", timeout=240)
def fcm231_slow_script(ctx):
    """a script still running after 10 s returns a receipt; the job is listed as a running `codemode` job, and its
    result arrives as a `<codemode>` message that wakes the session."""
    slow = "const r = await tools.shell({ command: \"sleep 14; echo late\" })\nreturn \"result: \" + r.output.trim()"
    ctx.set_script([
        {"tools": [execute(slow, "call_slow")]},
        {"text": "Finished the step."},
        {"text": "Received the result."},
    ])
    ctx.make_project(config_extra=FLAG)
    ctx.start()
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "run the slow script", timeout=60)
    _script(ctx, sid, mark)
    ctx.call("job.list.after", "GET", "/api/experimental/job")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events(filt=QUIET)
    ctx.record_llm()


@scenario("fcm231", timeout=240)
def fcm231_background_request(ctx):
    """a session-wide background request moves a running script at once: the call returns its receipt, the job is
    listed as a running `codemode` job, and the result arrives later as a `<codemode>` message. (Whether the request's
    own notice lands before or after the receipt is a race in both binaries, so only the parts are recorded.)"""
    slow = "const r = await tools.shell({ command: \"sleep 6; echo late\" })\nreturn \"result: \" + r.output.trim()"
    ctx.set_script([
        {"tools": [execute(slow, "call_moved")]},
        {"text": "Finished the step."},
        {"text": "Received the result."},
        {"text": "Received the result."},
    ])
    ctx.make_project(config_extra=FLAG)
    ctx.start()
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.prompt(sid, "run the script", wait=False)
    started = ctx.wait_for("session.tool.progress", start=mark, timeout=30)
    if started is None:
        ctx.rec("TIMEOUT waiting for the script", sid)
    ctx.call("session.background", "POST", f"/api/session/{sid}/background")
    ctx.wait_for("session.tool.success", start=mark, timeout=30)
    ctx.call("job.list", "GET", "/api/experimental/job")
    ev = ctx.wait_for("session.inbox.enqueued", start=mark, timeout=60,
                      pred=lambda e: "<codemode" in str(e.get("data", {})))
    if ev is None:
        ctx.rec("TIMEOUT waiting for the <codemode> message", sid)
    ctx.wait_idle(sid, start=ctx.server.events.index(ev) if ev else None, timeout=60)
    messages = ctx.data("messages", "GET", f"/api/session/{sid}/message", record=False)
    parts = [part for m in messages if m.get("type") == "assistant" for part in m.get("content", [])
             if part.get("type") == "tool"]
    ctx.rec("execute", parts)
    ctx.rec("synthetic", [m for m in messages if m.get("type") == "synthetic"])
    ctx.call("job.list.after", "GET", "/api/experimental/job")

"""opencode 2.2.7 tool changes: read's negative and past-the-end offsets, close-name suggestions and `skill://` files;
shell's `Exit code: N` notices and truncation; webfetch and Code Mode `fetch` without the URL fragment or credentials;
Code Mode `import`/`require`; `session_move` asking for `external_directory`; and the background job list
(`GET /api/experimental/job`)."""
import time

from parity.loop import run_prompt
from parity.registry import scenario


def call(name, args, id):
    return {"name": name, "args": args, "id": id}


def execute(code, id):
    return call("execute", {"code": code}, id)


@scenario("tools227")
def tools227_read(ctx):
    """read: a negative offset, an offset past the end, a typo with suggestions, a negative offset on a folder, and a
    built-in skill's in-memory files (`skill://opencode/SKILL.md`, its folder, an unknown skill)."""
    files = {"five.txt": "one\ntwo\nthree\nfour\nfive\n", "notes/config.ts": "x\n", "notes/db.ts": "y\n"}
    run_prompt(ctx, [
        {"tools": [call("read", {"path": "five.txt", "offset": -3}, "call_negative")]},
        {"tools": [call("read", {"path": "five.txt", "offset": 9, "limit": 2}, "call_past_end")]},
        {"tools": [call("read", {"path": "notes/conifg.ts"}, "call_typo")]},
        {"tools": [call("read", {"path": "notes", "offset": -1}, "call_folder")]},
        {"tools": [call("read", {"path": "skill://opencode/SKILL.md", "offset": 1, "limit": 3}, "call_skill")]},
        {"tools": [call("read", {"path": "skill://opencode/"}, "call_skill_root")]},
        {"tools": [call("read", {"path": "skill://nope/SKILL.md"}, "call_skill_missing")]},
        {"tools": [call("skill", {"id": "report"}, "call_skill_tool")]},
        {"text": "Done."},
    ], text="read them", files=files, config_extra={"permission": {"read": "allow", "skill": "allow"}})


@scenario("tools227")
def tools227_shell(ctx):
    """shell: a failing exit, a timeout (how long it waited and the killed process's exit code), a truncated tail that
    starts mid-line, and a single line longer than the limit."""
    run_prompt(ctx, [
        {"tools": [call("shell", {"command": "echo body; exit 7"}, "call_exit")]},
        {"tools": [call("shell", {"command": "echo start; sleep 5", "timeout": 500}, "call_timeout")]},
        {"tools": [call("shell", {"command": "head -c 60000 /dev/zero | tr '\\0' x; echo; echo end"}, "call_partial")]},
        {"tools": [call("shell", {"command": "head -c 120000 /dev/zero | tr '\\0' y"}, "call_line")]},
        {"text": "Done."},
    ], text="run them", config_extra={"permission": {"shell": "allow"}})


@scenario("tools227")
def tools227_webfetch(ctx):
    """webfetch: rules match the URL without its fragment, a redirect target loses its fragment before the check, and
    a URL with credentials is refused before anything is sent."""
    base = f"http://127.0.0.1:{ctx.mock.port}"
    ctx.mock.routes["/page"] = (200, "fetched page\n")
    ctx.mock.routes["/hop"] = (302, "", {"location": "/page#frag"})
    permission = {"webfetch": {"*": "deny", f"{base}/page": "allow", f"{base}/hop": "allow"}}
    run_prompt(ctx, [
        {"tools": [call("webfetch", {"url": f"{base}/page#top", "format": "text"}, "call_fragment")]},
        {"tools": [call("webfetch", {"url": f"{base}/hop", "format": "text"}, "call_hop")]},
        {"tools": [call("webfetch", {"url": f"http://user:pw@127.0.0.1:{ctx.mock.port}/page", "format": "text"},
                        "call_credentials")]},
        {"text": "Done."},
    ], text="fetch them", config_extra={"permission": permission})
    ctx.rec("fetched", [r["path"] for r in ctx.mock.requests if r["kind"] == "GET"])


@scenario("tools227")
def tools227_codemode(ctx):
    """Code Mode: `import`/`require` of built-in globals by Node module name, a module that does not exist, an
    export, and `fetch` without the fragment, with credentials refused and a redirect followed and checked."""
    base = f"http://127.0.0.1:{ctx.mock.port}"
    ctx.mock.routes["/page"] = (200, "fetched page\n")
    ctx.mock.routes["/hop"] = (302, "", {"location": "/page#frag"})
    ctx.mock.routes["/blocked"] = (302, "", {"location": "/secret"})
    modules = "\n".join([
        'import { URL } from "url"',
        'import * as buffer from "node:buffer"',
        'const util = require("util")',
        'let missing; try { require("lodash") } catch (e) { missing = e.message }',
        'let dynamic; try { await import("left-pad") } catch (e) { dynamic = e.message }',
        'return { path: new URL("https://a.test/x/y?q=1").pathname, buffer: Object.keys(buffer).sort(),',
        '  util: typeof util.TextEncoder, missing, dynamic, same: require("node:util") === util }',
    ])
    fetches = "\n".join([
        f'const page = await fetch("{base}/page#top")',
        f'let credentials; try {{ await fetch("http://u@127.0.0.1:{ctx.mock.port}/page") }} catch (e) {{ credentials = [e.name, e.message] }}',
        f'const hop = await fetch("{base}/hop")',
        f'let denied; try {{ await fetch("{base}/blocked") }} catch (e) {{ denied = e.message }}',
        'return { url: page.url, text: await page.text(), credentials, hop: [hop.url, hop.redirected, await hop.text()], denied }',
    ])
    permission = {"webfetch": {"*": "allow", f"{base}/secret": "deny"}}
    run_prompt(ctx, [
        {"tools": [execute(modules, "call_modules")]},
        {"tools": [execute("export const x = 1", "call_export")]},
        {"tools": [execute(fetches, "call_fetch")]},
        {"text": "Done."},
    ], text="run code", config_extra={"permission": permission})
    ctx.rec("fetched", [r["path"] for r in ctx.mock.requests if r["kind"] == "GET"])


@scenario("tools227")
def tools227_session_move(ctx):
    """session_move asks for external_directory before leaving the project (denied here), while a folder inside the
    project moves the session without asking; relative paths resolve against the session's location."""
    run_prompt(ctx, [
        {"tools": [execute('return await tools.opencode.session_move({ directory: "../elsewhere" })', "call_outside")]},
        {"tools": [execute('return await tools.opencode.session_move({ directory: " sub " })', "call_inside")]},
        {"text": "Done."},
    ], text="move", files={"sub/keep.txt": "x\n"},
        config_extra={"permission": {"external_directory": "deny"}})


@scenario("tools227", timeout=120)
def tools227_job_list(ctx):
    """GET /api/experimental/job while a background shell runs, with filters and an unknown session, then after its
    notice was delivered."""
    ctx.set_script([
        {"tools": [call("shell", {"command": "sleep 3; echo finished", "background": True}, "call_background")]},
        {"text": "Started."},
        {"match": "finished", "text": "Noted."},
    ])
    ctx.make_project(config_extra={"permission": {"shell": "allow"}})
    ctx.start()
    sid = ctx.create_session()
    ctx.prompt(sid, "start it")
    ctx.call("job.list", "GET", "/api/experimental/job")
    ctx.call("job.list.session", "GET", f"/api/experimental/job?sessionID={sid}&descendants=true")
    ctx.call("job.list.kind", "GET", "/api/experimental/job?kind=subagent")
    ctx.call("job.list.status", "GET", "/api/experimental/job?status=running")
    ctx.call("job.list.unknown", "GET", "/api/experimental/job?sessionID=ses_missing")
    ctx.call("job.list.invalid", "GET", "/api/experimental/job?kind=nope")
    # The completion notice wakes the session again after the first turn ended.
    deadline = time.time() + 30
    while time.time() < deadline and len(ctx.mock.main_requests()) < 3:
        time.sleep(0.2)
    mark = len(ctx.server.events)
    ctx.server.wait_event(lambda e: e.get("type") == "session.execution.succeeded"
                          and e.get("data", {}).get("sessionID") == sid, timeout=30, start=max(0, mark - 50))
    time.sleep(1)
    ctx.call("job.list.after", "GET", "/api/experimental/job")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()

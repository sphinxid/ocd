"""Automations (on by default since 2.4.1; `experimental.automations` through `OPENCODE_FLAGS=automations` in 2.3.1):
the automation HTTP API and its errors, unattended creation over HTTP, a prompt automation firing, `/automation` and the
automation tools through Code Mode and directly, code actions (state, sends, invalid programs, pauses), shell actions,
two servers on one data directory running an automation once, and automations whose folder is gone staying manageable
where `automations: false`.

Times differ between runs (epoch-ms values are masked); automations that run use short `at` delays or the 15 s code
minimum, so these scenarios take a while."""
import glob
import json
import os
import shutil
import sqlite3
import sys
import time

from parity.harness import Server
from parity.registry import scenario

HERE = os.path.dirname(os.path.abspath(__file__))
ALLOW = {"permission": {"*": "allow"}}
MISSING_SESSION = "ses_000000000000aaaaaaaaaaaaaa"
MISSING_AUTOMATION = "aut_000000000000aaaaaaaaaaaaaa"
HOURLY = {"type": "every", "every": "1 hour"}


def config(extra=None, timezone="America/New_York", allow=True):
    cfg = {}
    if timezone:
        cfg["timezone"] = timezone
    if allow:
        cfg.update(ALLOW)
    cfg.update(extra or {})
    return cfg


def boot(ctx, script=None, flags=None, **kw):
    ctx.set_script(script or [])
    if flags:
        ctx.env_extra["OPENCODE_FLAGS"] = flags
    else:
        ctx.env_extra.pop("OPENCODE_FLAGS", None)
    ctx.make_project(config_extra=config(**kw))
    ctx.start()


def create(ctx, label, body):
    return ctx.call(f"create.{label}", "POST", "/api/experimental/automation", body)


def automations(ctx, label, query=""):
    return ctx.call(label, "GET", f"/api/experimental/automation{query}")


def created_id(created):
    return created.get("data", {}).get("id") if isinstance(created, dict) else None


def wait_fired(ctx, count=1, timeout=90, sids=None):
    """Waits until `count` automation prompts were admitted (a durable prompt event carries `metadata.automation`),
    then until their sessions are idle."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        fired = [e for e in ctx.events() if e.get("type") == "session.inbox.enqueued"
                 and (((e.get("data") or {}).get("item") or {}).get("payload") or {}).get("metadata", {}).get("automation")]
        if len(fired) >= count:
            break
        time.sleep(0.2)
    else:
        ctx.rec("TIMEOUT waiting for automation prompts", count)
        return
    for sid in sids or []:
        ctx.wait_idle(sid, timeout=timeout)


def wait_status(ctx, aid, done, timeout=90):
    """Polls an automation until `done(info)` holds."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, data = ctx.call("poll", "GET", f"/api/experimental/automation/{aid}", record=False)
        info = data.get("data") if isinstance(data, dict) else None
        if isinstance(info, dict) and done(info):
            return info
        time.sleep(0.25)
    ctx.rec("TIMEOUT waiting for automation", aid)
    return None


def finish(ctx, sid):
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("automation", hold_for_title=0)
def automation_api(ctx):
    """create/list/get/pause/resume/delete over HTTP, the trigger, target and service errors, the list filters, a
    sessionless automation in a directory, and the command list."""
    boot(ctx)
    sid = ctx.create_session(title="Automations")
    other = os.path.join(ctx.project, "other")
    os.makedirs(other)
    _, created = create(ctx, "every", {"sessionID": sid, "name": "build", "trigger": HOURLY, "expires": "in 8 hours",
                                        "action": {"type": "prompt", "prompt": "check the build"}})
    aid = created_id(created)
    create(ctx, "at.relative", {"sessionID": sid, "name": "soon", "trigger": {"type": "at", "at": "in 20 minutes", "tz": "Europe/Paris"},
                                "action": {"type": "prompt", "prompt": "later"}})
    create(ctx, "at.wall", {"sessionID": sid, "name": "wall", "trigger": {"type": "at", "at": "2099-11-02T09:30"},
                            "action": {"type": "prompt", "prompt": "later"}})
    create(ctx, "cron", {"sessionID": sid, "name": "standup", "trigger": {"type": "cron", "cron": "0 9 * * 1-5"}, "expires": "2099-01-01T00:00:00Z",
                         "action": {"type": "prompt", "prompt": "standup", "to": {"subagent": "general"}}})
    create(ctx, "code", {"sessionID": sid, "name": "quiet", "trigger": {"type": "every", "every": "15 seconds"},
                         "action": {"type": "code", "code": "return", "args": {"channel": "#ops"}}})
    create(ctx, "shell", {"sessionID": sid, "name": "status", "trigger": HOURLY, "action": {"type": "shell", "command": "git status"}})
    _, standalone = create(ctx, "standalone", {"name": "standalone", "directory": ctx.project, "trigger": HOURLY,
                                               "action": {"type": "prompt", "prompt": "hello", "to": {"session": sid}}})

    def body(**extra):
        b = {"sessionID": sid, "name": "x", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x"}}
        b.update(extra)
        return b

    for label, b in [
        ("at.past", body(trigger={"type": "at", "at": "2020-01-01T00:00:00Z"})),
        ("at.invalid", body(trigger={"type": "at", "at": "tomorrow"})),
        ("at.zero", body(trigger={"type": "at", "at": "in 0 seconds"})),
        ("every.invalid", body(trigger={"type": "every", "every": "soon"})),
        ("every.zero", body(trigger={"type": "every", "every": "0 seconds"})),
        ("every.short", body(trigger={"type": "every", "every": "30 seconds"})),
        ("every.code.short", body(trigger={"type": "every", "every": "10 seconds"}, action={"type": "code", "code": "return"})),
        ("cron.seconds", body(trigger={"type": "cron", "cron": "* * * * * *"})),
        ("cron.never", body(trigger={"type": "cron", "cron": "0 0 31 2 *"})),
        ("cron.invalid", body(trigger={"type": "cron", "cron": "61 * * * *"})),
        ("tz.unknown", body(trigger={"type": "every", "every": "1 hour", "tz": "Mars/Base"})),
        ("expires.before", body(trigger={"type": "at", "at": "in 2 hours"}, expires="in 1 hour")),
        ("expires.invalid", body(expires="whenever")),
        ("event.off", body(trigger={"type": "event", "server": "ci", "event": "build.changed"})),
        ("trigger.untyped", body(trigger={"every": "1 hour"})),
        ("target.both", body(action={"type": "prompt", "prompt": "x", "to": {"session": sid, "subagent": "general"}})),
        ("target.other", body(action={"type": "prompt", "prompt": "x", "to": {"session": MISSING_SESSION}})),
        ("subagent.unknown", body(action={"type": "prompt", "prompt": "x", "to": {"subagent": "nobody"}})),
        ("subagent.primary", body(action={"type": "prompt", "prompt": "x", "to": {"subagent": "build"}})),
        ("subagent.hidden", body(action={"type": "prompt", "prompt": "x", "to": {"subagent": "title"}})),
        ("name.empty", body(name="")),
        ("prompt.empty", body(action={"type": "prompt", "prompt": ""})),
        ("session.missing", body(sessionID=MISSING_SESSION)),
        ("sessionless.prompt", {"name": "x", "directory": ctx.project, "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x"}}),
        ("sessionless.subagent", {"name": "x", "directory": ctx.project, "trigger": HOURLY,
                                  "action": {"type": "prompt", "prompt": "x", "to": {"subagent": "general"}}}),
        ("sessionless.code", {"name": "x", "directory": ctx.project, "trigger": HOURLY, "action": {"type": "code", "code": "return"}}),
        ("sessionless.shell", {"name": "x", "directory": ctx.project, "trigger": HOURLY, "action": {"type": "shell", "command": "echo x"}}),
        ("shell.empty", body(action={"type": "shell", "command": ""})),
        ("every.shell.short", body(trigger={"type": "every", "every": "30 seconds"}, action={"type": "shell", "command": "echo x"})),
        ("directory.relative", {"name": "x", "directory": "relative", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x", "to": {"session": sid}}}),
        ("directory.gone", {"name": "x", "directory": os.path.join(ctx.project, "gone"), "trigger": HOURLY,
                            "action": {"type": "prompt", "prompt": "x", "to": {"session": sid}}}),
    ]:
        create(ctx, label, b)
    automations(ctx, "list")
    automations(ctx, "list.session", f"?sessionID={sid}")
    automations(ctx, "list.directory", f"?directory={ctx.project}")
    automations(ctx, "list.other", f"?directory={other}")
    automations(ctx, "list.relative", "?directory=relative")
    automations(ctx, "list.missing", f"?sessionID={MISSING_SESSION}")
    if aid:
        ctx.call("get", "GET", f"/api/experimental/automation/{aid}")
        ctx.call("pause", "POST", f"/api/experimental/automation/{aid}/pause")
        ctx.call("pause.again", "POST", f"/api/experimental/automation/{aid}/pause")
        ctx.call("resume", "POST", f"/api/experimental/automation/{aid}/resume")
        ctx.call("resume.again", "POST", f"/api/experimental/automation/{aid}/resume")
        ctx.call("delete", "DELETE", f"/api/experimental/automation/{aid}")
        ctx.call("get.deleted", "GET", f"/api/experimental/automation/{aid}")
        ctx.call("delete.deleted", "DELETE", f"/api/experimental/automation/{aid}")
    ctx.call("get.unknown", "GET", f"/api/experimental/automation/{MISSING_AUTOMATION}")
    ctx.call("get.bad", "GET", "/api/experimental/automation/nope")
    if created_id(standalone):
        ctx.call("get.standalone", "GET", f"/api/experimental/automation/{created_id(standalone)}")
    automations(ctx, "list.after")
    ctx.settle(f"/api/command?directory={ctx.project}")
    commands = ctx.data("commands", "GET", "/api/command", record=False)
    ctx.rec("commands", [c for c in commands if c.get("name") == "automation"] if isinstance(commands, list) else commands)
    ctx.record_events()


@scenario("automation", hold_for_title=0)
def automation_api_unattended(ctx):
    """HTTP creation settles permission asks unattended: a code action the rules would ask for is forbidden, a subagent
    the rules would ask for is refused."""
    boot(ctx, allow=False, extra={"permission": {"*": "allow", "execute": "ask", "subagent": "ask"}})
    sid = ctx.create_session(title="Automations")
    create(ctx, "code.ask", {"sessionID": sid, "name": "blocked", "trigger": HOURLY, "action": {"type": "code", "code": "return"}})
    create(ctx, "subagent.ask", {"sessionID": sid, "name": "child", "trigger": HOURLY,
                                 "action": {"type": "prompt", "prompt": "x", "to": {"subagent": "general"}}})
    automations(ctx, "list", f"?sessionID={sid}")


@scenario("automation", hold_for_title=0)
def automation_config_off(ctx):
    """2.4.1: automations are on without any config, `experimental.automations`/`experimental.schedules` no longer
    count, and the top-level `automations: false` turns them off where the automation runs (list empty, a session or
    directory filter and creation forbidden)."""
    boot(ctx, extra={"experimental": {"schedules": False, "automations": False}})
    sid = ctx.create_session(title="Off")
    create(ctx, "default", {"sessionID": sid, "name": "default", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x"}})
    automations(ctx, "list", f"?sessionID={sid}")
    ctx.stop()
    with open(os.path.join(ctx.project, "opencode.json")) as f:
        cfg = json.load(f)
    cfg.pop("experimental", None)
    cfg["automations"] = False
    with open(os.path.join(ctx.project, "opencode.json"), "w") as f:
        json.dump(cfg, f)
    ctx.start()
    automations(ctx, "list.off")
    automations(ctx, "list.off.session", f"?sessionID={sid}")
    automations(ctx, "list.off.directory", f"?directory={ctx.project}")
    create(ctx, "off", {"sessionID": sid, "name": "off", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x"}})
    ctx.settle(f"/api/command?directory={ctx.project}")
    commands = ctx.data("commands", "GET", "/api/command", record=False)
    ctx.rec("commands", [c for c in commands if c.get("name") == "automation"] if isinstance(commands, list) else commands)


@scenario("automation", hold_for_title=0, timeout=240)
def automation_folder_gone(ctx):
    """with `automations: false`, an automation whose folder is gone stays manageable (listed, read, paused and
    deleted), while one in a folder that still exists reads as forbidden."""
    boot(ctx)
    gone = os.path.join(ctx.project, "worktree")
    os.makedirs(gone)
    _, made = ctx.call("session.gone", "POST", "/api/session", {"title": "Gone", "location": {"directory": gone}})
    sid = made.get("data", {}).get("id") if isinstance(made, dict) else None
    _, a = create(ctx, "gone", {"name": "gone", "directory": gone, "trigger": HOURLY,
                                "action": {"type": "prompt", "prompt": "x", "to": {"session": sid}}})
    kept = ctx.create_session(title="Kept")
    _, b = create(ctx, "kept", {"name": "kept", "directory": ctx.project, "trigger": HOURLY,
                                "action": {"type": "prompt", "prompt": "x", "to": {"session": kept}}})
    ctx.stop()
    shutil.rmtree(gone)
    with open(os.path.join(ctx.project, "opencode.json")) as f:
        cfg = json.load(f)
    cfg["automations"] = False
    with open(os.path.join(ctx.project, "opencode.json"), "w") as f:
        json.dump(cfg, f)
    ctx.start()
    automations(ctx, "list")
    automations(ctx, "list.gone", f"?directory={gone}")
    automations(ctx, "list.project", f"?directory={ctx.project}")
    for label, aid in [("gone", created_id(a)), ("kept", created_id(b))]:
        if aid:
            ctx.call(f"get.{label}", "GET", f"/api/experimental/automation/{aid}")
            ctx.call(f"pause.{label}", "POST", f"/api/experimental/automation/{aid}/pause")
            ctx.call(f"delete.{label}", "DELETE", f"/api/experimental/automation/{aid}")
    automations(ctx, "list.after")


@scenario("automation", hold_for_title=0, timeout=240)
def automation_fires(ctx):
    """an `at` automation sends its prompt into the creating session (steer, `metadata.automation`), the model
    answers, and the automation is done."""
    boot(ctx, script=[{"text": "Build checked."}])
    sid = ctx.create_session(title="Automations")
    _, created = create(ctx, "at", {"sessionID": sid, "name": "soon", "trigger": {"type": "at", "at": "in 2 seconds"},
                                    "action": {"type": "prompt", "prompt": "check the build"}})
    wait_fired(ctx, sids=[sid])
    aid = created_id(created)
    if aid:
        wait_status(ctx, aid, lambda i: i.get("status") == "done")
        ctx.call("get.after", "GET", f"/api/experimental/automation/{aid}")
    finish(ctx, sid)


@scenario("automation", hold_for_title=0, timeout=240)
def automation_subagent(ctx):
    """a `{subagent}` target starts a child of the creating session with the agent for the run; the child answers
    and reports to no one."""
    boot(ctx, script=[{"text": "Triaged."}])
    sid = ctx.create_session(title="Automations")
    _, created = create(ctx, "child", {"sessionID": sid, "name": "triage", "trigger": {"type": "at", "at": "in 2 seconds"},
                                       "action": {"type": "prompt", "prompt": "triage the inbox", "to": {"subagent": "general"}}})
    aid = created_id(created)
    wait_fired(ctx)
    if aid:
        info = wait_status(ctx, aid, lambda i: i.get("status") == "done")
        child = f"ses_{aid}_1"
        ctx.wait_idle(child, timeout=60)
        ctx.call("child", "GET", f"/api/session/{child}")
        ctx.call("child.messages", "GET", f"/api/session/{child}/message")
        ctx.call("get.after", "GET", f"/api/experimental/automation/{aid}")
        del info
    time.sleep(2)
    ctx.call("parent.messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm()


@scenario("automation", timeout=240)
def automation_command(ctx):
    """/automation hands the request to the model with the automation instructions; the model creates an automation
    through Code Mode (`automation.create`), lists it, and the automation then fires."""
    code = ('const a = await tools.automation.create({ name: "stretch", trigger: { type: "at", at: "in 3 seconds" }, '
            'action: { type: "prompt", prompt: "Time to stretch." } }); const l = await tools.automation.list({}); '
            'return [a.status, a.trigger.type, a.tz, a.action.to, l.automations.length]')
    boot(ctx, script=[
        {"tools": [{"name": "execute", "id": "call_1", "args": {"code": code}}]},
        {"text": "Stretch reminder in 3 seconds."},
        {"text": "Stretched."},
    ])
    sid = ctx.create_session()
    mark = len(ctx.server.events)
    ctx.call("command", "POST", f"/api/session/{sid}/command", {"name": "automation", "text": "  in 3 seconds remind me to stretch  "})
    ctx.wait_idle(sid, start=mark)
    wait_fired(ctx, sids=[sid])
    automations(ctx, "list", f"?sessionID={sid}")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("automation", timeout=240)
def automation_tools_direct(ctx):
    """the automation tools without Code Mode (`execute` denied): their summaries, input errors, another project's
    automation reading as missing, and pause/resume/delete."""
    boot(ctx, script=[
        {"tools": [{"name": "automation_create", "id": "call_1",
                    "args": {"name": "ci", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "check CI"}}}]},
        {"tools": [{"name": "automation_create", "id": "call_2",
                    "args": {"name": "watch", "trigger": {"type": "cron", "cron": "0 9 * * 1-5"}, "expires": "2099-01-01T00:00:00Z",
                             "action": {"type": "prompt", "prompt": "x" * 100, "to": {"subagent": "general"}}}}]},
        {"tools": [{"name": "automation_create", "id": "call_3",
                    "args": {"name": "x", "trigger": {"every": "1 hour"}, "action": {"type": "prompt", "prompt": "two"}}}]},
        {"tools": [{"name": "automation_create", "id": "call_4",
                    "args": {"name": "x", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x", "to": {"session": "ses_1", "subagent": "general"}}}}]},
        {"tools": [{"name": "automation_create", "id": "call_5",
                    "args": {"name": "", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "x"}}}]},
        {"tools": [{"name": "automation_create", "id": "call_6",
                    "args": {"name": "x", "trigger": {"type": "every", "every": "soon"}, "action": {"type": "prompt", "prompt": "x"}}}]},
        {"tools": [{"name": "automation_create", "id": "call_7",
                    "args": {"name": "x", "trigger": {"type": "event", "server": "ci", "event": "e"}, "action": {"type": "prompt", "prompt": "x"}}}]},
        {"tools": [{"name": "automation_list", "id": "call_8", "args": {}}]},
        {"tools": [{"name": "automation_pause", "id": "call_9", "args": {"id": MISSING_AUTOMATION}}]},
        {"tools": [{"name": "automation_delete", "id": "call_10", "args": {"id": "nope"}}]},
        {"text": "Done."},
    ], extra={"permission": {"*": "allow", "execute": "deny"}})
    sid = ctx.create_session(title="Automations")
    ctx.prompt(sid, "make some automations", timeout=180)
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
    ctx.record_events()
    ctx.record_llm(full=True)


@scenario("automation", hold_for_title=0, timeout=300)
def automation_checks(ctx):
    """code actions over HTTP: programs refused at creation (syntax, dynamic or aliased tools, `require("tools")`,
    unknown, shell and automation tools, session moves), a check that keeps state and hands one prompt to the creator, a check that
    starts a quiet child, and one that returns an invalid shape and pauses."""
    server = os.path.join(HERE, "fake_mcp.py")
    boot(ctx, script=[{"text": "Noted."}, {"text": "Child noted."}],
         extra={"mcp": {"servers": {"fake": {"type": "local", "command": [sys.executable, server]}}}})
    sid = ctx.create_session(title="Checks")
    deadline = time.time() + 20
    while time.time() < deadline:
        _, _, data = ctx.server.request("GET", f"/api/mcp?directory={ctx.project}", headers={"x-opencode-directory": ctx.project})
        if "connected" in json.dumps(data):
            break
        time.sleep(0.2)
    fast = {"type": "every", "every": "15 seconds"}
    ids = {}
    for label, code in [
        ("syntax", "const x = {"),
        ("dynamic", "return await tools.fake[name]({})"),
        ("alias", "const t = tools.fake; return await t.good_one({})"),
        ("require", 'const t = require("tools"); return await t.fake.good_one({})'),
        ("unknown", "return await tools.fake.missing({})"),
        ("shell", 'await tools.shell({ command: "pwd" })'),
        ("forbidden", "return await tools.automation.create({})"),
        ("move", "return await tools.opencode.session.move({})"),
        ("empty", "   "),
        ("send", 'const r = await tools.fake.good_one({}); console.log("said " + r); '
                 'if (state === null) return { state: { runs, said: r } }; return { done: true, send: { prompt: "Fake said " + r } }'),
        ("child", 'return { done: true, send: { prompt: "Look into it", to: { subagent: "general" } } }'),
        ("shape", 'return "not-a-result"'),
        ("throws", 'throw new Error("check failed")'),
    ]:
        _, created = create(ctx, f"code.{label}", {"sessionID": sid, "name": label, "trigger": fast, "action": {"type": "code", "code": code}})
        if created_id(created):
            ids[label] = created_id(created)
    automations(ctx, "list.created", f"?sessionID={sid}")
    for label, done in [("send", lambda i: i.get("status") == "done"), ("child", lambda i: i.get("status") == "done"),
                        ("shape", lambda i: i.get("status") == "paused"), ("throws", lambda i: (i.get("lastOutcome") == "failed"))]:
        if label in ids:
            wait_status(ctx, ids[label], done, timeout=120)
    if "send" in ids:
        ctx.wait_idle(sid, timeout=60)
    if "child" in ids:
        ctx.wait_idle(f"ses_{ids['child']}_1", timeout=60)
    for label, aid in ids.items():
        if label != "throws":
            ctx.call(f"get.{label}", "GET", f"/api/experimental/automation/{aid}")
        else:
            ctx.call(f"pause.{label}", "POST", f"/api/experimental/automation/{aid}/pause")
    finish(ctx, sid)


def _lease_rows(ctx):
    rows = []
    for path in sorted(glob.glob(os.path.join(ctx.state, "data", "opencode", "*.db"))):
        con = sqlite3.connect(path)
        try:
            rows.extend(con.execute("SELECT key, value FROM kv WHERE key = 'automation:leader'").fetchall())
        finally:
            con.close()
    return [{"key": k, "value": json.loads(v)} for k, v in rows]


@scenario("automation", hold_for_title=0, timeout=240)
def automation_lease(ctx):
    """two servers on one data directory: the lease on KV `automation:leader` lets exactly one of them run an
    automation; the lease is held while an automation is active and released when none is."""
    boot(ctx, script=[{"text": "Fired once."}, {"text": "Fired twice?"}])
    second = Server(ctx.binary, ctx.project, ctx.env(), ctx.name + "-2")
    try:
        sid = ctx.create_session(title="Lease")
        ctx.rec("lease.idle", _lease_rows(ctx))
        _, created = create(ctx, "at", {"sessionID": sid, "name": "once", "trigger": {"type": "at", "at": "in 3 seconds"},
                                        "action": {"type": "prompt", "prompt": "fire once"}})
        _, hourly = create(ctx, "every", {"sessionID": sid, "name": "later", "trigger": HOURLY, "action": {"type": "prompt", "prompt": "later"}})
        time.sleep(1)
        ctx.rec("lease.active", [{"key": r["key"], "fields": sorted(r["value"])} for r in _lease_rows(ctx)])
        wait_fired(ctx, sids=[sid])
        time.sleep(4)
        _, data = ctx.call("messages", "GET", f"/api/session/{sid}/message", record=False)
        users = [m for m in (data.get("data", []) if isinstance(data, dict) else []) if m.get("type") == "user"]
        ctx.rec("user.messages", [m.get("text") for m in users])
        automations(ctx, "list", f"?sessionID={sid}")
        if created_id(created):
            ctx.call("get.fired", "GET", f"/api/experimental/automation/{created_id(created)}")
        if created_id(hourly):
            ctx.call("delete.hourly", "DELETE", f"/api/experimental/automation/{created_id(hourly)}")
        deadline = time.time() + 70
        while time.time() < deadline and _lease_rows(ctx):
            time.sleep(1)
        ctx.rec("lease.released", _lease_rows(ctx))
    finally:
        second.close()
    ctx.record_events()


@scenario("automation", hold_for_title=0, timeout=240)
def automation_shell(ctx):
    """2.4.1 shell actions: created over HTTP (asks settled unattended: allowed by config, refused where `shell` asks),
    each run starts the fixed command without a model and records `Exit code: N` with its output, a failing command
    reads as failed, and the creating session gets no prompt."""
    boot(ctx, extra={"permission": {"*": "allow", "shell": {"*": "allow", "rm *": "ask"}}})
    sid = ctx.create_session(title="Shell")
    soon = {"type": "at", "at": "in 2 seconds"}
    _, ok = create(ctx, "ok", {"sessionID": sid, "name": "ok", "trigger": soon, "action": {"type": "shell", "command": "echo automation-output"}})
    _, bad = create(ctx, "bad", {"sessionID": sid, "name": "bad", "trigger": soon, "action": {"type": "shell", "command": "echo failing; exit 7"}})
    create(ctx, "asks", {"sessionID": sid, "name": "asks", "trigger": soon, "action": {"type": "shell", "command": "rm -rf build"}})
    create(ctx, "outside", {"sessionID": sid, "name": "outside", "trigger": soon, "action": {"type": "shell", "command": "cd ..; ls"}})
    for label, created in [("ok", ok), ("bad", bad)]:
        aid = created_id(created)
        if aid:
            wait_status(ctx, aid, lambda i: "lastOutput" in i and i.get("status") == "done")
            ctx.call(f"get.{label}", "GET", f"/api/experimental/automation/{aid}")
    automations(ctx, "list", f"?sessionID={sid}")
    ctx.call("messages", "GET", f"/api/session/{sid}/message")
